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

use std::sync::{Arc, Mutex, OnceLock};

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
///
/// `text` 是**完整匹配文本**：对 `file:line:col` 这类带位置后缀的路径，
/// `text` 含后缀，打开前用 [`Link::path_text`] 取纯路径。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub kind: LinkKind,
    pub start: u16,
    pub end: u16,
    pub text: String,
    /// `file:line:col` 的行号（仅 Path；`text` 仍含后缀）。
    pub line: Option<u32>,
    /// `file:line:col` 的列号（仅 Path）。
    pub col: Option<u32>,
}

impl Link {
    #[inline]
    pub fn contains(&self, col: u16) -> bool {
        self.start <= col && col <= self.end
    }

    /// 构造一条链接（插件 [`Linkifier`] 用；位置后缀留空）。
    pub fn new(kind: LinkKind, start: u16, end: u16, text: String) -> Self {
        Self { kind, start, end, text, line: None, col: None }
    }

    /// 去掉 `:line[:col]` 位置后缀后的纯路径；非 Path 返回全文。
    pub fn path_text(&self) -> &str {
        strip_path_position(&self.text)
    }
}

/// 去掉 `file:line:col` / `file:line` 的位置后缀，返回纯路径。
pub fn strip_path_position(text: &str) -> &str {
    let (path, _, _) = split_position(text);
    path
}

/// 只剥离位置后缀，不做“像不像路径”的判断。
fn split_position(text: &str) -> (&str, Option<u32>, Option<u32>) {
    let mut rest = text;
    let mut first = None;
    if let Some((prefix, n)) = trailing_num(rest) {
        first = Some(n);
        rest = prefix;
    }
    // 两级后缀：右为列、左为行；一级后缀按“行号”处理。
    match trailing_num(rest) {
        Some((prefix, line)) => (prefix, Some(line), first),
        None => (rest, first, None),
    }
}

/// 末尾 `:<数字>`；返回（前缀，数值）。多于 7 位视为普通文本而不是行号。
fn trailing_num(s: &str) -> Option<(&str, u32)> {
    let idx = s.rfind(':')?;
    let digits = s.get(idx + 1..)?;
    if digits.is_empty() || digits.len() > 7 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((&s[..idx], digits.parse().ok()?))
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
        let (line, col) = if kind == LinkKind::Path {
            match parse_path(&text) {
                Some(pos) => pos,
                None => {
                    i += 1;
                    continue;
                },
            }
        } else {
            (None, None)
        };
        out.push(Link {
            kind,
            start: i as u16,
            end: end as u16,
            text,
            line,
            col,
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

/// 路径起点比 URL 更严格：只接受行首/空白/开括号/引号。
///
/// 这能避免把 `--prefix=/usr`、`PATH=/bin:/usr/bin`、提示符
/// `user@host:/tmp$` 里的斜杠识别成可点击路径。URL 仍可按 `key=https://…`
/// 这种形式命中（URL 的边界规则更宽）。
fn is_path_boundary(chars: &[char], i: usize) -> bool {
    match i.checked_sub(1).map(|p| chars[p]) {
        None => true,
        Some(prev) => {
            prev.is_whitespace() || matches!(prev, '(' | '[' | '{' | '<' | '"' | '\'' | '`')
        },
    }
}

/// 紧贴在路径起点前的引号（`"…"` / `'…'` / `` `…` ``）。
///
/// 引号内允许空格，这是“带空格路径”的识别入口；只有**紧贴**起点的引号才算，
/// 正文里随便一个引号不会把整段文字变成路径。
fn quote_at(chars: &[char], i: usize) -> Option<char> {
    match i.checked_sub(1).map(|p| chars[p]) {
        Some(q @ ('"' | '\'' | '`')) => Some(q),
        _ => None,
    }
}

/// `~user` 用户名允许的字符。
fn is_user_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// 扫描一个候选路径 token 的长度（不含引号壳）。
///
/// 起点比 URL 宽：普通文件名（grep/编译器输出 `src/main.rs:12:3`）也先收集，
/// 真正的形状校验在 [`parse_path`] 里做。行尾标点仍由 [`trim_end`] 去掉。
fn path_len(chars: &[char], i: usize) -> Option<usize> {
    if !is_path_boundary(chars, i) {
        return None;
    }
    let next = chars.get(i + 1).copied();
    let next2 = chars.get(i + 2).copied();
    let starts = match chars.get(i) {
        // `//` 是 scheme 分隔符，不当作路径起点
        Some('/') => next != Some('/'),
        // `~/…`（HOME）或 `~user[/…]`
        Some('~') => next == Some('/') || next.is_some_and(is_user_char),
        Some('.') => next == Some('/') || (next == Some('.') && next2 == Some('/')),
        // 相对文件名：只有后面带 `:line` 时才会被 parse_path 接受。
        Some(c) => c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '~'),
        None => false,
    };
    if !starts {
        return None;
    }
    let quote = quote_at(chars, i);
    let mut len = 0;
    while i + len < chars.len() {
        let c = chars[i + len];
        match quote {
            Some(q) => {
                if c == q || c.is_control() {
                    break;
                }
            },
            None => {
                if c.is_whitespace()
                    || matches!(
                        c,
                        '"' | '\'' | '<' | '>' | '|' | '*' | '?' | ';' | ',' | ')'
                    )
                {
                    break;
                }
            },
        }
        len += 1;
    }
    (len > 0).then_some(len)
}

/// 校验候选路径并拆出 `:line:col` 后缀。
///
/// 接受：绝对路径、`~/…`、`~user[/…]`、`./`/`../` 相对路径，以及
/// grep/编译器风格“相对文件名 + `:line`”（`src/main.rs:12:3`、`foo.rs:7`）。
fn parse_path(text: &str) -> Option<(Option<u32>, Option<u32>)> {
    let (path, line, col) = split_position(text);
    if path.len() < 2 || path.starts_with("//") || path.starts_with("/~") {
        return None;
    }
    let shaped = path.starts_with('/')
        || path.starts_with("~/")
        || path.starts_with("./")
        || path.starts_with("../")
        || (path.starts_with('~') && valid_user_path(path))
        || (line.is_some() && looks_like_relative_file(path));
    shaped.then_some((line, col))
}

/// `~user[/path]`：用户名非空且只含 [`is_user_char`]。
fn valid_user_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('~') else { return false };
    let user = rest.split('/').next().unwrap_or_default();
    !user.is_empty() && user.chars().all(is_user_char)
}

/// grep / 编译器输出里的相对文件名，至少要求字母与 `.` 或 `/`。
fn looks_like_relative_file(path: &str) -> bool {
    if path.is_empty() || path.starts_with('-') || path.starts_with('.') && path.len() == 1 {
        return false;
    }
    let base = path.rsplit('/').next().unwrap_or(path);
    let sane = path.chars().all(|c| {
        c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '+' | '@' | '~' | '\\')
    });
    let has_alpha = path.chars().any(|c| c.is_alphabetic());
    // 扩展名必须带字母：`v1.2.3:4` 这类版本文本不能被当成“文件第 4 行”。
    let ext_ok = base
        .rsplit_once('.')
        .is_some_and(|(_, ext)| !ext.is_empty() && ext.len() <= 16 && ext.chars().any(|c| c.is_ascii_alphabetic()));
    sane && has_alpha && (ext_ok || path.contains('/'))
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

/// 插件 Linkifier 扩展点：在内置识别**没有命中**时补充自定义链接。
///
/// 这是纯识别层扩展，不参与打开策略：插件返回的链接仍要经过 app 的
/// scheme 白名单与本地路径存在性检查。实现必须是线程安全的（点击/hover
/// 发生在 UI 线程，但注册可能来自插件加载线程）。
pub trait Linkifier: Send + Sync {
    /// 输入是一行可见文本；返回的 `Link.start/end` 按**字符**下标（闭区间）。
    fn scan(&self, line: &str) -> Vec<Link>;
}

static LINKIFIERS: OnceLock<Mutex<Vec<Arc<dyn Linkifier>>>> = OnceLock::new();

fn linkifiers() -> &'static Mutex<Vec<Arc<dyn Linkifier>>> {
    LINKIFIERS.get_or_init(|| Mutex::new(Vec::new()))
}

fn lock_linkifiers() -> std::sync::MutexGuard<'static, Vec<Arc<dyn Linkifier>>> {
    // 插件 Linkifier 里 panic 不应让“识别链接”永久失效。
    linkifiers().lock().unwrap_or_else(|poison| poison.into_inner())
}

/// 注册插件 Linkifier；同一份 `Arc` 不重复注册。返回是否新增。
pub fn register_linkifier(linkifier: Arc<dyn Linkifier>) -> bool {
    let mut list = lock_linkifiers();
    if list.iter().any(|l| Arc::ptr_eq(l, &linkifier)) {
        return false;
    }
    list.push(linkifier);
    true
}

/// 卸载全部插件 Linkifier（插件重载 / 测试清理）。
pub fn clear_linkifiers() {
    lock_linkifiers().clear();
}

/// 在内置规则无命中时运行已注册的插件 Linkifier；顺序按注册顺序。
pub fn scan_custom(line: &str) -> Vec<Link> {
    // 克隆 Arc 列表后立刻释放锁：插件的 scan 回调里如果再 register/clear
    // linkifier，不能在同一线程重入非可重入 Mutex（会死锁）。
    let list: Vec<Arc<dyn Linkifier>> = {
        let guard = lock_linkifiers();
        guard.iter().cloned().collect()
    };
    list.iter().flat_map(|linkifier| linkifier.scan(line)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    struct Reentrant;

    impl Linkifier for Reentrant {
        fn scan(&self, _line: &str) -> Vec<Link> {
            // 旧实现持锁回调，这里会死锁。
            let _ = register_linkifier(Arc::new(Noop));
            Vec::new()
        }
    }

    struct Noop;
    impl Linkifier for Noop {
        fn scan(&self, _line: &str) -> Vec<Link> {
            Vec::new()
        }
    }

    #[test]
    fn linkifier_callback_can_register_without_deadlock() {
        clear_linkifiers();
        register_linkifier(Arc::new(Reentrant));
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = scan_custom("https://example.com");
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(2))
            .expect("scan_custom deadlocked while holding the linkifier lock");
        clear_linkifiers();
    }

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
        // 括号包裹的路径：右括号是外壳，不属于路径。
        assert_eq!(texts("at (/tmp/a)"), vec!["/tmp/a"]);
    }

    #[test]
    fn path_requires_a_real_boundary_not_embedded_in_arguments() {
        // 命令参数/环境变量里的 `/` 后缀不是“当前行明显路径”，避免误点。
        assert!(texts("--prefix=/usr/local").is_empty());
        assert!(texts("--config=/etc/app.conf").is_empty());
        assert!(texts("PATH=/bin:/usr/bin").is_empty());
        assert!(texts("user@host:/tmp$ ").is_empty(), "提示符里的 cwd 不该变成链接");
        assert!(texts("key=value/with/slash").is_empty());
        // 空格/行首/开引号之后的路径仍然命中。
        assert_eq!(texts("open /tmp/a"), vec!["/tmp/a"]);
        assert_eq!(texts("\"/tmp/b\""), vec!["/tmp/b"]);
        assert_eq!(texts("(/tmp/c)"), vec!["/tmp/c"]);
        // URL 不受路径边界收紧影响：`key=https://…` 仍可点击。
        assert_eq!(texts("url=https://example.com/x"), vec!["https://example.com/x"]);
    }

    #[test]
    fn http_ipv4_with_port_is_a_url_not_a_bare_ip() {
        let found = scan("http://127.0.0.1:8080/health");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, LinkKind::Url);
        assert_eq!(found[0].text, "http://127.0.0.1:8080/health");
        // 裸 IP:port 只识别为 Ipv4（UI 可以选择不打开），不会混成 Path/URL。
        let bare = scan("listen 127.0.0.1:8080");
        assert_eq!(bare.len(), 1);
        assert_eq!(bare[0].kind, LinkKind::Ipv4);
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
    fn quoted_paths_allow_spaces() {
        assert_eq!(texts(r#"open "/tmp/my notes.txt""#), vec!["/tmp/my notes.txt"]);
        assert_eq!(texts(r#"see '/home/u/a b/c.log'"#), vec!["/home/u/a b/c.log"]);
        // 引号里不是路径的句子不该变成链接。
        assert!(texts(r#""hello world""#).is_empty());
        // 反引号同样按路径壳处理（markdown/日志常见）。
        assert_eq!(texts("`/var/tmp/a b.csv`"), vec!["/var/tmp/a b.csv"]);
    }

    #[test]
    fn tilde_user_paths() {
        assert_eq!(texts("cd ~alice/work"), vec!["~alice/work"]);
        assert_eq!(texts("home ~bob"), vec!["~bob"]);
        // `~` 单独出现不算链接；`~/` 仍是 HOME。
        assert!(texts("~ done").is_empty());
        assert_eq!(texts("cd ~/x"), vec!["~/x"]);
    }

    #[test]
    fn file_line_col_suffix() {
        let found = scan("src/main.rs:12:3: error[E0308]");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, LinkKind::Path);
        assert_eq!(found[0].text, "src/main.rs:12:3");
        assert_eq!(found[0].path_text(), "src/main.rs");
        assert_eq!((found[0].line, found[0].col), (Some(12), Some(3)));
        // 只有行号时 line=Some、col=None。
        let line_only = scan("foo.rs:7: bad");
        assert_eq!(line_only.len(), 1);
        assert_eq!(line_only[0].path_text(), "foo.rs");
        assert_eq!((line_only[0].line, line_only[0].col), (Some(7), None));
        // 绝对路径同样支持位置后缀。
        let abs = scan("/tmp/a.rs:1:2");
        assert_eq!(abs[0].path_text(), "/tmp/a.rs");
        // 没有行号后缀的裸文件名不识别；版本/时间不是文件行号。
        assert!(texts("main.rs").is_empty());
        assert!(texts("12:34:56 done").is_empty());
        assert!(texts("v1.2.3:4").is_empty());
        assert_eq!(strip_path_position("/tmp/a.rs:12:3"), "/tmp/a.rs");
        assert_eq!(strip_path_position("/tmp/a.rs:12"), "/tmp/a.rs");
        assert_eq!(strip_path_position("/tmp/a:b.rs"), "/tmp/a:b.rs");
    }

    #[test]
    fn plugin_linkifiers_extend_scan_without_touching_builtin() {
        use std::sync::Arc;
        struct AtLink;
        impl Linkifier for AtLink {
            fn scan(&self, line: &str) -> Vec<Link> {
                line.find("@@").map(|i| {
                    Link::new(LinkKind::Path, i as u16, (i + 1) as u16, "@@plugin".to_string())
                }).into_iter().collect()
            }
        }
        use crate::Frame;
        clear_linkifiers();
        let at = Arc::new(AtLink);
        assert!(register_linkifier(at.clone()));
        assert!(!register_linkifier(at), "同一份 Arc 不重复注册");
        assert_eq!(scan_custom("see @@plugin").len(), 1);
        // 内置 scan 不因插件注册而改变。
        assert!(scan("see @@plugin").is_empty());
        // 插件在 Frame::link_at 的内置规则无命中时兜底。
        let mut f = Frame::new(20, 1);
        put_frame_text(&mut f, "see @@plugin");
        assert_eq!(
            f.link_at(0, 4),
            Some((LinkKind::Path, "@@plugin".to_string()))
        );
        clear_linkifiers();
        assert!(scan_custom("see @@plugin").is_empty());
    }

    /// link.rs 没有 Frame helper，单独造一行。
    fn put_frame_text(f: &mut crate::Frame, text: &str) {
        use crate::CellData;
        for (col, ch) in text.chars().enumerate() {
            let mut cell = CellData::default();
            cell.ch = ch as u32;
            f.put(col, cell, None);
        }
    }

    #[test]
    fn times_and_versions_are_not_links() {
        assert!(texts("12:34:56 done").is_empty());
        assert!(texts("v1.2.3").is_empty());
    }
}
