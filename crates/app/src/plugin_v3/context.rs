// SPDX-License-Identifier: GPL-3.0-only
//! v3 插件命令的 `when` 上下文表达式。
//!
//! 清单里 `[[commands]] when = "terminalFocus && !settingsOpen"` 只描述**可用条件**，
//! 不执行任何插件代码；求值在 UI 线程用当前 `CommandContext` 完成。语法刻意保持
//! 最小：`&&` / `||` / `!` / 括号 / `变量 op 整数`（`< <= > >= == !=`）。
//!
//! 未知变量在清单校验阶段就报错（拼错不会被静默当成 false），求值阶段再遇到未知
//! 变量按 false 处理，保证宿主升级/降级不会 panic。

use std::fmt;

/// 宿主提供给插件命令的只读上下文快照（UI 线程构造）。
///
/// 字段名与清单 `when` 里的变量一一对应：`terminal_focus` → `terminalFocus`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommandContext {
    /// 终端持有键盘焦点（没有设置页/面板/弹窗抢走焦点）。
    pub terminal_focus: bool,
    pub settings_open: bool,
    pub palette_open: bool,
    pub tab_search_open: bool,
    pub sidebar_open: bool,
    pub tab_count: usize,
    pub pane_count: usize,
    /// 当前 Tab 已固定（Pin）。
    pub active_tab_pinned: bool,
}

impl CommandContext {
    /// 支持的变量名（文档、清单校验、错误信息共用）。
    pub const KEYS: [&'static str; 9] = [
        "terminalFocus",
        "settingsOpen",
        "paletteOpen",
        "tabSearchOpen",
        "sidebarOpen",
        "tabCount",
        "paneCount",
        "activeTabPinned",
        "true",
    ];

    fn resolve(&self, name: &str) -> Option<Value> {
        Some(match name {
            "terminalFocus" => Value::Bool(self.terminal_focus),
            "settingsOpen" => Value::Bool(self.settings_open),
            "paletteOpen" => Value::Bool(self.palette_open),
            "tabSearchOpen" => Value::Bool(self.tab_search_open),
            "sidebarOpen" => Value::Bool(self.sidebar_open),
            "tabCount" => Value::Int(self.tab_count as i64),
            "paneCount" => Value::Int(self.pane_count as i64),
            "activeTabPinned" => Value::Bool(self.active_tab_pinned),
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return None,
        })
    }
}

impl CommandContext {
    /// 变量是否存在（校验用；`false` 也是合法变量）。
    pub fn is_known(name: &str) -> bool {
        name == "false" || Self::KEYS.contains(&name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Value {
    Bool(bool),
    Int(i64),
}

impl Value {
    fn truthy(self) -> bool {
        match self {
            Value::Bool(value) => value,
            Value::Int(value) => value != 0,
        }
    }

    fn as_int(self) -> i64 {
        match self {
            Value::Bool(value) => i64::from(value),
            Value::Int(value) => value,
        }
    }
}

/// 比较运算符。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

impl CmpOp {
    fn eval(self, left: i64, right: i64) -> bool {
        match self {
            CmpOp::Eq => left == right,
            CmpOp::Ne => left != right,
            CmpOp::Gt => left > right,
            CmpOp::Ge => left >= right,
            CmpOp::Lt => left < right,
            CmpOp::Le => left <= right,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Expr {
    Always,
    Ident(String),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Cmp(String, CmpOp, i64),
}

/// 解析后的 `when` 表达式；空字符串 = 总是可用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhenExpr {
    expr: Expr,
}

impl WhenExpr {
    /// 解析（含未知变量检查）。空/纯空白返回 [`Expr::Always`]。
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(Self { expr: Expr::Always });
        }
        let mut parser = Parser {
            tokens: tokenize(text)?,
            position: 0,
            source: text,
            depth: 0,
        };
        let expr = parser.or_expr()?;
        if let Some(token) = parser.peek() {
            return Err(format!("when 表达式多余内容 {token:?}: {text}"));
        }
        Ok(Self { expr })
    }

    pub fn eval(&self, ctx: &CommandContext) -> bool {
        eval(&self.expr, ctx)
    }

    pub fn is_empty(&self) -> bool {
        self.expr == Expr::Always
    }
}

impl fmt::Display for WhenExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", render(&self.expr))
    }
}

fn render(expr: &Expr) -> String {
    match expr {
        Expr::Always => "true".to_string(),
        Expr::Ident(name) => name.clone(),
        Expr::Not(inner) => format!("!{}", render(inner)),
        Expr::And(left, right) => format!("{} && {}", render(left), render(right)),
        Expr::Or(left, right) => format!("{} || {}", render(left), render(right)),
        Expr::Cmp(name, op, value) => format!("{name} {} {value}", op.as_str()),
    }
}

impl CmpOp {
    fn as_str(self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
        }
    }
}

fn eval(expr: &Expr, ctx: &CommandContext) -> bool {
    match expr {
        Expr::Always => true,
        Expr::Ident(name) => ctx.resolve(name).is_some_and(Value::truthy),
        Expr::Not(inner) => !eval(inner, ctx),
        Expr::And(left, right) => eval(left, ctx) && eval(right, ctx),
        Expr::Or(left, right) => eval(left, ctx) || eval(right, ctx),
        Expr::Cmp(name, op, value) => ctx
            .resolve(name)
            .is_some_and(|left| op.eval(left.as_int(), *value)),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Ident(String),
    Int(i64),
    Not,
    And,
    Or,
    LParen,
    RParen,
    Cmp(CmpOp),
}

fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            c if c.is_whitespace() => {
                i += 1;
            }
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '!' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Token::Cmp(CmpOp::Ne));
                    i += 2;
                } else {
                    tokens.push(Token::Not);
                    i += 1;
                }
            }
            '&' => {
                if chars.get(i + 1) != Some(&'&') {
                    return Err(format!("when 表达式只支持 &&（位置 {i}）: {text}"));
                }
                tokens.push(Token::And);
                i += 2;
            }
            '|' => {
                if chars.get(i + 1) != Some(&'|') {
                    return Err(format!("when 表达式只支持 ||（位置 {i}）: {text}"));
                }
                tokens.push(Token::Or);
                i += 2;
            }
            '=' => {
                if chars.get(i + 1) != Some(&'=') {
                    return Err(format!("when 表达式只支持 ==（位置 {i}）: {text}"));
                }
                tokens.push(Token::Cmp(CmpOp::Eq));
                i += 2;
            }
            '>' | '<' => {
                let ge = chars.get(i + 1) == Some(&'=');
                let op = match (ch, ge) {
                    ('>', false) => CmpOp::Gt,
                    ('>', true) => CmpOp::Ge,
                    ('<', false) => CmpOp::Lt,
                    _ => CmpOp::Le,
                };
                tokens.push(Token::Cmp(op));
                i += if ge { 2 } else { 1 };
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let raw: String = chars[start..i].iter().collect();
                let value = raw
                    .parse::<i64>()
                    .map_err(|_| format!("when 表达式数字过大 {raw:?}: {text}"))?;
                tokens.push(Token::Int(value));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                if !CommandContext::is_known(&name) {
                    return Err(format!(
                        "when 表达式未知变量 {name:?}（支持：{}）",
                        CommandContext::KEYS.join(", ")
                    ));
                }
                tokens.push(Token::Ident(name));
            }
            other => {
                return Err(format!("when 表达式非法字符 {other:?}（位置 {i}）: {text}"));
            }
        }
    }
    Ok(tokens)
}

struct Parser<'a> {
    tokens: Vec<Token>,
    position: usize,
    source: &'a str,
    /// `unary_expr` 的递归深度；`!`/括号过深必须在 UI 线程栈溢出前拒绝。
    depth: usize,
}

const MAX_WHEN_DEPTH: usize = 64;

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position).cloned();
        if token.is_some() {
            self.position += 1;
        }
        token
    }

    fn expect(&mut self, token: Token) -> Result<(), String> {
        match self.next() {
            Some(found) if found == token => Ok(()),
            Some(found) => Err(format!(
                "when 表达式需要 {token:?}，实际 {found:?}: {}",
                self.source
            )),
            None => Err(format!(
                "when 表达式意外结束（缺少 {token:?}）: {}",
                self.source
            )),
        }
    }

    fn or_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.and_expr()?;
        while self.peek() == Some(&Token::Or) {
            self.next();
            let right = self.and_expr()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.unary_expr()?;
        while self.peek() == Some(&Token::And) {
            self.next();
            let right = self.unary_expr()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary_expr(&mut self) -> Result<Expr, String> {
        if self.depth >= MAX_WHEN_DEPTH {
            return Err(format!(
                "when 表达式嵌套超过 {MAX_WHEN_DEPTH} 层: {}",
                self.source
            ));
        }
        self.depth += 1;
        let result = self.unary_expr_inner();
        self.depth -= 1;
        result
    }

    fn unary_expr_inner(&mut self) -> Result<Expr, String> {
        match self.next() {
            Some(Token::Not) => Ok(Expr::Not(Box::new(self.unary_expr()?))),
            Some(Token::LParen) => {
                let expr = self.or_expr()?;
                self.expect(Token::RParen)?;
                Ok(expr)
            }
            Some(Token::Ident(name)) => {
                if let Some(Token::Cmp(op)) = self.peek().cloned() {
                    self.next();
                    let value = match self.next() {
                        Some(Token::Int(value)) => value,
                        Some(found) => {
                            return Err(format!(
                                "when 表达式比较需要整数，实际 {found:?}: {}",
                                self.source
                            ))
                        }
                        None => return Err(format!("when 表达式比较缺少整数: {}", self.source)),
                    };
                    Ok(Expr::Cmp(name, op, value))
                } else {
                    Ok(Expr::Ident(name))
                }
            }
            Some(Token::Int(value)) => Ok(Expr::Cmp("true".to_string(), CmpOp::Eq, value)),
            Some(found) => Err(format!("when 表达式意外的 {found:?}: {}", self.source)),
            None => Err(format!("when 表达式为空: {}", self.source)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> CommandContext {
        CommandContext {
            terminal_focus: true,
            settings_open: false,
            palette_open: false,
            tab_search_open: false,
            sidebar_open: false,
            tab_count: 3,
            pane_count: 1,
            active_tab_pinned: false,
        }
    }

    #[test]
    fn empty_when_always_matches() {
        let expr = WhenExpr::parse("").unwrap();
        assert!(expr.is_empty());
        assert!(expr.eval(&CommandContext::default()));
    }

    #[test]
    fn booleans_and_negation() {
        assert!(WhenExpr::parse("terminalFocus").unwrap().eval(&ctx()));
        assert!(!WhenExpr::parse("!terminalFocus").unwrap().eval(&ctx()));
        assert!(WhenExpr::parse("terminalFocus && !settingsOpen")
            .unwrap()
            .eval(&ctx()));
        assert!(!WhenExpr::parse("settingsOpen || paletteOpen")
            .unwrap()
            .eval(&ctx()));
        assert!(WhenExpr::parse("true")
            .unwrap()
            .eval(&CommandContext::default()));
        assert!(!WhenExpr::parse("false")
            .unwrap()
            .eval(&CommandContext::default()));
    }

    #[test]
    fn integers_and_comparisons() {
        let ctx = ctx();
        assert!(WhenExpr::parse("tabCount > 1").unwrap().eval(&ctx));
        assert!(!WhenExpr::parse("tabCount > 3").unwrap().eval(&ctx));
        assert!(WhenExpr::parse("paneCount >= 1 && tabCount <= 3")
            .unwrap()
            .eval(&ctx));
        assert!(WhenExpr::parse("tabCount == 3").unwrap().eval(&ctx));
        assert!(WhenExpr::parse("tabCount != 2").unwrap().eval(&ctx));
    }

    #[test]
    fn precedence_and_parentheses() {
        let ctx = ctx();
        // false && false || true == true（&& 优先）
        let expr = WhenExpr::parse("settingsOpen && paletteOpen || terminalFocus").unwrap();
        assert!(expr.eval(&ctx));
        let expr = WhenExpr::parse("settingsOpen && (paletteOpen || terminalFocus)").unwrap();
        assert!(!expr.eval(&ctx));
    }

    #[test]
    fn unknown_or_malformed_is_rejected() {
        let err = WhenExpr::parse("termnalFocus").unwrap_err();
        assert!(err.contains("未知变量"), "{err}");
        assert!(WhenExpr::parse("tabCount >").is_err());
        assert!(WhenExpr::parse("terminalFocus & settingsOpen").is_err());
        assert!(WhenExpr::parse("(terminalFocus").is_err());
        assert!(WhenExpr::parse("terminalFocus settingsOpen").is_err());
        assert_eq!(
            WhenExpr::parse("tabCount > 1").unwrap().to_string(),
            "tabCount > 1"
        );
    }
}

#[cfg(test)]
mod depth_tests {
    use super::WhenExpr;

    #[test]
    fn deeply_nested_when_is_rejected_not_stack_overflow() {
        let text = format!("{}terminalFocus{}", "(".repeat(10_000), ")".repeat(10_000));
        let err = WhenExpr::parse(&text).unwrap_err();
        assert!(err.contains("嵌套"), "{err}");
        let text = format!("{}terminalFocus", "!".repeat(10_000));
        assert!(WhenExpr::parse(&text).is_err());
    }
}
