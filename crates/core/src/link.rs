// SPDX-License-Identifier: GPL-3.0-only
//! 终端行内链接识别：URL / IPv4[:端口] / 文件路径。
//!
//! 纯函数、不依赖 UI：输入是一行可见文本，输出是若干**列区间**。
//! UI 层用 [`is_allowed_uri`] 做打开前的安全白名单，Ctrl+点击打开。
//!
//! 列号按**字符**计数（0 基、闭区间），本层不感知显示宽度；URL path 可以
//! 含中文等宽字符。需要显示列映射时用 [`Frame::url_at`]
//! （它会按单元格把显示列换算成字符下标，宽字符/组合字符不会错位）。
//!
//! [`Frame::url_at`]: crate::Frame::url_at

/// 链接类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    /// `http://` / `https://` / `file://`（只识别这三种可安全打开的 scheme）
    Url,
    /// 四段点分十进制，可带端口
    Ipv4,
    /// `/abs/path`、`~/home/path`、`./relative`、`../up`
    Path,
}

/// 一处命中。`start..=end` 是字符下标（闭区间）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub kind: LinkKind,
    pub start: u16,
    pub end: u16,
    pub text: String,
}

impl Link {
    #[inline]
    pub fn contains(&self, col: u16) -> bool {
        self.start <= col && col <= self.end
    }
}

/// 行尾常粘上的标点：识别时去掉，但括号要配对处理。
/// 含中日韩全角标点，否则 `https://example.com。` 会把句号也带进 URL。
const TRAILING: &[char] = &[
    '.', ',', ';', ':', '!', '?', '>', '<', '\'', '"', ']', '}', '，', '。', '；', '：', '！',
    '？', '、', '）', '】', '》', '」', '』',
];

/// URL 结束符：空白/控制字符、HTML 包裹符与中日韩全角标点。
/// 半角括号留给 [`trim_end`] 做配对；全角标点在中日文里通常直接断句，不再收进 URL。
fn is_url_end(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || matches!(
            c,
            '<' | '>' | '"' | '`' | '，' | '。' | '；' | '：' | '！' | '？' | '、' | '）'
                | '】' | '》' | '」' | '』'
        )
}

pub fn scan(text: &str) -> Vec<Link> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let Some((len, kind)) = match_at(&chars, i) else {
            i += 1;
            continue;
        };
        let end = trim_end(&chars, i, i + len - 1, kind);
        if end < i {
            i += 1;
            continue;
        }
        let text: String = chars[i..=end].iter().collect();
        // 去掉标点后 `https://` / `https://.` 这类空壳不再是链接。
        if kind == LinkKind::Url && !valid_url_text(&text) {
            i += 1;
            continue;
        }
        out.push(Link {
            kind,
            start: i as u16,
            end: end as u16,
            text,
        });
        i = end + 1;
    }
    out
}

/// 从 `i` 开始尝试匹配；返回（长度，类型）。
fn match_at(chars: &[char], i: usize) -> Option<(usize, LinkKind)> {
    if !is_boundary(chars, i) {
        return None;
    }
    scheme_len(chars, i)
        .map(|n| (n, LinkKind::Url))
        .or_else(|| ipv4_len(chars, i).map(|n| (n, LinkKind::Ipv4)))
        .or_else(|| path_len(chars, i).map(|n| (n, LinkKind::Path)))
}

/// 前一个字符必须是边界，避免把 `xhttps://…`、`a/usr/bin` 当成链接。
fn is_boundary(chars: &[char], i: usize) -> bool {
    match i.checked_sub(1).map(|p| chars[p]) {
        None => true,
        // 只拦 ASCII 词中粘连（`xhttps://`）；中文正文常无空格，`见https://…` 应当命中。
        Some(prev) => {
            !(prev.is_ascii_alphanumeric()
                || prev == '_'
                || prev == '/'
                || prev == '-'
                || prev == '.')
        },
    }
}

fn scheme_len(chars: &[char], i: usize) -> Option<usize> {
    // 只识别可以安全交给系统打开器的 scheme；`javascript:` / `data:` /
    // `ftp:` 等不进入识别结果（[`is_allowed_uri`] 还会在打开前再验证一次）。
    const SCHEMES: [&str; 3] = ["https://", "http://", "file://"];
    let rest: String = chars[i..].iter().take(8).collect();
    let scheme = SCHEMES
        .iter()
        .find(|s| rest.get(..s.len()).is_some_and(|p| p.eq_ignore_ascii_case(s)))?;
    let mut len = scheme.len();
    while i + len < chars.len() && !is_url_end(chars[i + len]) {
        len += 1;
    }
    // 至少一个 URI 字符串；真正的非空校验在 trim 后由 valid_url_text 做。
    (len > scheme.len()).then_some(len)
}

/// `scheme://` 之后必须还有内容（`file:///` 的 `/` 也算）。
fn valid_url_text(text: &str) -> bool {
    let Some((scheme, rest)) = text.split_once("://") else { return false };
    !rest.is_empty() && matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https" | "file")
}

/// 打开前的安全白名单：只允许 `http` / `https` / `file`，且必须带 `://`。
///
/// OSC 8 的 URI 由终端应用任意指定，不能直接交给 `xdg-open`：
/// `javascript:`、`data:`、`mailto:`、裸命令等都拒绝。控制字符/空白也拒绝，
/// 避免把换行或额外参数混进外部打开器。
pub fn is_allowed_uri(uri: &str) -> bool {
    if uri.is_empty() || uri.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    valid_url_text(uri)
}

fn ipv4_len(chars: &[char], i: usize) -> Option<usize> {
    let mut idx = i;
    for octet in 0..4 {
        let start = idx;
        while idx < chars.len() && chars[idx].is_ascii_digit() {
            idx += 1;
        }
        let digits = idx - start;
        if !(1..=3).contains(&digits) {
            return None;
        }
        let value: u32 = chars[start..idx].iter().collect::<String>().parse().ok()?;
        if value > 255 {
            return None;
        }
        if octet < 3 {
            if chars.get(idx) != Some(&'.') {
                return None;
            }
            idx += 1;
        }
    }
    // 可选的 :port
    if chars.get(idx) == Some(&':') {
        let start = idx + 1;
        let mut p = start;
        while p < chars.len() && chars[p].is_ascii_digit() {
            p += 1;
        }
        if p > start && p - start <= 5 {
            idx = p;
        }
    }
    // 不能是更长数字串的一部分；但句尾的 "." 不算（会被 trim 掉）
    match chars.get(idx) {
        Some(c) if c.is_ascii_digit() => return None,
        Some('.') if chars.get(idx + 1).is_some_and(|c| c.is_ascii_digit()) => return None,
        _ => {},
    }
    Some(idx - i)
}

fn path_len(chars: &[char], i: usize) -> Option<usize> {
    let next = chars.get(i + 1).copied();
    let next2 = chars.get(i + 2).copied();
    let starts = match chars.get(i) {
        // `//` 是 scheme 分隔符，不当作路径起点
        Some('/') => next != Some('/'),
        Some('~') => next == Some('/'),
        Some('.') => next == Some('/') || (next == Some('.') && next2 == Some('/')),
        _ => false,
    };
    if !starts {
        return None;
    }
    let mut len = 0;
    while i + len < chars.len() {
        let c = chars[i + len];
        if c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '|' | '*' | '?' | ';' | ',' | ')') {
            break;
        }
        len += 1;
    }
    // 必须长得像路径：绝对路径 / home / 相对路径；单个 `/`、`./` 不算
    let candidate: String = chars[i..i + len].iter().collect();
    let looks_like_path = candidate.starts_with('/')
        || candidate.starts_with("~/")
        || candidate.starts_with("./")
        || candidate.starts_with("../");
    (len >= 2 && looks_like_path).then_some(len)
}

/// 去掉尾随标点；`)` 只在括号不配对时才去掉。
fn trim_end(chars: &[char], start: usize, mut end: usize, kind: LinkKind) -> usize {
    while end > start {
        let c = chars[end];
        if TRAILING.contains(&c) {
            end -= 1;
            continue;
        }
        if c == ')' && kind != LinkKind::Path {
            let slice: String = chars[start..=end].iter().collect();
            let open = slice.matches('(').count();
            let close = slice.matches(')').count();
            if close > open {
                end -= 1;
                continue;
            }
        }
        break;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(line: &str) -> Vec<String> {
        scan(line).into_iter().map(|l| l.text).collect()
    }

    #[test]
    fn finds_https_with_trailing_period() {
        assert_eq!(texts("see https://example.com/a?b=1."), vec!["https://example.com/a?b=1"]);
    }

    #[test]
    fn only_http_https_file_schemes_are_recognized() {
        assert_eq!(texts("HTTP://Example.com/x"), vec!["HTTP://Example.com/x"]);
        assert_eq!(texts("file:///tmp/a.txt"), vec!["file:///tmp/a.txt"]);
        // 老实现识别 ftp；打开白名单只允许三种，识别层也不再声称支持。
        assert!(texts("ftp://example.com/file").is_empty());
        assert!(texts("javascript:alert(1)").is_empty());
        assert!(texts("data://text/html,hello").is_empty());
        assert!(texts("mailto:user@example.com").is_empty());
    }

    #[test]
    fn trailing_punctuation_and_wrappers_are_trimmed() {
        assert_eq!(texts("see https://example.com/a."), vec!["https://example.com/a"]);
        assert_eq!(texts("see https://example.com/a,"), vec!["https://example.com/a"]);
        assert_eq!(texts("see https://example.com/a;"), vec!["https://example.com/a"]);
        assert_eq!(texts("see https://example.com/a!"), vec!["https://example.com/a"]);
        assert_eq!(texts("url <https://example.com/a>"), vec!["https://example.com/a"]);
        assert_eq!(texts("\"https://example.com/a\""), vec!["https://example.com/a"]);
        assert_eq!(texts("https://example.com/a。"), vec!["https://example.com/a"]);
        assert_eq!(texts("https://example.com/a，"), vec!["https://example.com/a"]);
        assert_eq!(texts("（https://example.com/a）"), vec!["https://example.com/a"]);
        // 中文正文没有空格：CJK 字符不是“词中粘连”，全角句号必须截断 URL。
        assert_eq!(texts("见https://example.com/a。更多"), vec!["https://example.com/a"]);
        assert_eq!(texts("链接：https://example.com/a，然后"), vec!["https://example.com/a"]);
        // 只有标点的空壳不能算链接
        assert!(texts("https://.").is_empty());
        assert!(texts("https://).").is_empty());
    }

    #[test]
    fn allowed_uri_is_a_strict_scheme_allowlist() {
        for ok in [
            "http://example.com",
            "https://example.com/a?b=c#d",
            "HTTPS://EXAMPLE.COM",
            "file:///tmp/a.txt",
            "file://server/share/a",
            "https://user:pass@example.com:8443/x",
        ] {
            assert!(is_allowed_uri(ok), "应允许: {ok}");
        }
        for bad in [
            "",
            "https://",
            "ftp://example.com",
            "javascript:alert(1)",
            "JavaScript://alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "mailto:user@example.com",
            "http:example.com",
            "http//example.com",
            "https://exa mple.com",
            "https://example.com\nrm -rf /",
            "https://example.com\u{0}",
        ] {
            assert!(!is_allowed_uri(bad), "应拒绝: {bad:?}");
        }
    }

    #[test]
    fn balanced_parentheses_are_kept() {
        // 维基风格链接：`)` 属于 URL
        assert_eq!(
            texts("https://en.wikipedia.org/wiki/Foo_(bar)"),
            vec!["https://en.wikipedia.org/wiki/Foo_(bar)"]
        );
        // 外层括号不属于 URL
        assert_eq!(texts("(https://example.com/x)"), vec!["https://example.com/x"]);
    }

    #[test]
    fn url_must_have_host() {
        assert!(texts("https://").is_empty());
        assert!(texts("xhttps://a.com").is_empty(), "词中不应命中");
    }

    #[test]
    fn ipv4_with_port_and_rejects_invalid() {
        assert_eq!(texts("listen 127.0.0.1:8080 ok"), vec!["127.0.0.1:8080"]);
        assert_eq!(texts("ping 10.0.0.1."), vec!["10.0.0.1"]);
        assert!(texts("999.1.1.1").is_empty());
        assert!(texts("1.2.3").is_empty());
        assert!(texts("version 1.2.3.4.5").is_empty(), "更长数字串不命中");
    }

    #[test]
    fn finds_paths() {
        assert_eq!(texts("open /usr/share/doc/readme"), vec!["/usr/share/doc/readme"]);
        assert_eq!(texts("cd ~/work/project"), vec!["~/work/project"]);
        assert_eq!(texts("run ./target/release/app"), vec!["./target/release/app"]);
        assert_eq!(texts("up ../sibling/file.rs"), vec!["../sibling/file.rs"]);
        assert!(texts("main.rs").is_empty());
        assert!(texts("and/or").is_empty(), "普通斜杠不该命中");
    }

    #[test]
    fn path_trailing_punctuation_trimmed() {
        assert_eq!(texts("see /var/log/syslog."), vec!["/var/log/syslog"]);
        assert_eq!(texts("file /etc/hosts:"), vec!["/etc/hosts"]);
    }

    #[test]
    fn multiple_links_in_order() {
        let found = scan("a https://x.io/p b 192.168.1.2 c /tmp/d");
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].kind, LinkKind::Url);
        assert_eq!(found[1].kind, LinkKind::Ipv4);
        assert_eq!(found[2].kind, LinkKind::Path);
        assert!(found[0].end < found[1].start && found[1].end < found[2].start);
    }

    #[test]
    fn columns_cover_the_text() {
        let line = "x https://a.io/y z";
        let link = &scan(line)[0];
        let slice: String = line.chars().skip(link.start as usize).take((link.end - link.start + 1) as usize).collect();
        assert_eq!(slice, "https://a.io/y");
        assert!(link.contains(link.start) && link.contains(link.end));
    }

    #[test]
    fn times_and_versions_are_not_links() {
        assert!(texts("12:34:56 done").is_empty());
        assert!(texts("v1.2.3").is_empty());
    }
}
