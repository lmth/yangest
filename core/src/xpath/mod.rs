// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! XPath 1.0 parser — produces a public `XPathExpr` AST.
//!
//! yangest is a schema-only tool: XPath is **never evaluated** against a data tree.
//! This parser exists only for:
//!
//! 1. Syntax validation of `when` and `must` expressions
//! 2. Prefix extraction (to detect unused imports and undefined prefixes)
//! 3. Function whitelist checking (RFC 7950 §10.5)
//! 4. Schema-path extraction for `path` (leafref) statements
//!
//! Reference: XPath 1.0 §1-4 (https://www.w3.org/TR/xpath-10/)

use std::collections::HashSet;
use std::fmt;

// ── Public AST types ──────────────────────────────────────────────────────────

/// A qualified name, e.g. `prefix:local` or bare `local`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QName {
    pub prefix: Option<String>,
    pub local: String,
}

impl QName {
    pub fn bare(local: impl Into<String>) -> Self {
        QName { prefix: None, local: local.into() }
    }
    pub fn qualified(prefix: impl Into<String>, local: impl Into<String>) -> Self {
        QName { prefix: Some(prefix.into()), local: local.into() }
    }
}

impl fmt::Display for QName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.prefix {
            Some(p) => write!(f, "{}:{}", p, self.local),
            None => write!(f, "{}", self.local),
        }
    }
}

/// XPath axis specifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Child,
    Attribute,
    Self_,
    Parent,
    Ancestor,
    AncestorOrSelf,
    Descendant,
    DescendantOrSelf,
    Following,
    FollowingSibling,
    Namespace,
    Preceding,
    PrecedingSibling,
}

impl Axis {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "child" => Some(Axis::Child),
            "attribute" => Some(Axis::Attribute),
            "self" => Some(Axis::Self_),
            "parent" => Some(Axis::Parent),
            "ancestor" => Some(Axis::Ancestor),
            "ancestor-or-self" => Some(Axis::AncestorOrSelf),
            "descendant" => Some(Axis::Descendant),
            "descendant-or-self" => Some(Axis::DescendantOrSelf),
            "following" => Some(Axis::Following),
            "following-sibling" => Some(Axis::FollowingSibling),
            "namespace" => Some(Axis::Namespace),
            "preceding" => Some(Axis::Preceding),
            "preceding-sibling" => Some(Axis::PrecedingSibling),
            _ => None,
        }
    }
}

/// Node test in a location step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeTest {
    Name(QName),
    Wildcard,
    PrefixWildcard(String),
    Node,
    Text,
    Comment,
    Pi(Option<String>),
}

/// One step in a location path.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub axis: Axis,
    pub node_test: NodeTest,
    pub predicates: Vec<XPathExpr>,
}

/// A location path.
#[derive(Debug, Clone, PartialEq)]
pub struct LocationPath {
    pub absolute: bool,
    pub steps: Vec<Step>,
}

/// The XPath expression AST.
#[derive(Debug, Clone, PartialEq)]
pub enum XPathExpr {
    String(String),
    Number(f64),
    Variable(QName),
    Path(LocationPath),
    Filter(Box<XPathExpr>, Vec<XPathExpr>),
    FunctionCall { name: QName, args: Vec<XPathExpr> },
    Union(Box<XPathExpr>, Box<XPathExpr>),
    Or(Box<XPathExpr>, Box<XPathExpr>),
    And(Box<XPathExpr>, Box<XPathExpr>),
    Eq(Box<XPathExpr>, Box<XPathExpr>),
    Ne(Box<XPathExpr>, Box<XPathExpr>),
    Lt(Box<XPathExpr>, Box<XPathExpr>),
    Gt(Box<XPathExpr>, Box<XPathExpr>),
    Le(Box<XPathExpr>, Box<XPathExpr>),
    Ge(Box<XPathExpr>, Box<XPathExpr>),
    Add(Box<XPathExpr>, Box<XPathExpr>),
    Sub(Box<XPathExpr>, Box<XPathExpr>),
    Mul(Box<XPathExpr>, Box<XPathExpr>),
    Div(Box<XPathExpr>, Box<XPathExpr>),
    Mod(Box<XPathExpr>, Box<XPathExpr>),
    Neg(Box<XPathExpr>),
}

// ── Parse error ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XPathError {
    pub message: String,
    pub offset: usize,
}

impl XPathError {
    fn new(message: impl Into<String>, offset: usize) -> Self {
        XPathError { message: message.into(), offset }
    }
}

impl fmt::Display for XPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "XPath parse error at offset {}: {}", self.offset, self.message)
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

pub fn parse_xpath(input: &str) -> Result<XPathExpr, XPathError> {
    let mut p = XParser::new(input);
    let expr = p.parse_expr()?;
    p.skip_ws();
    if p.pos < p.input.len() {
        return Err(XPathError::new(
            format!("unexpected input after expression: '{}'", &p.input[p.pos..]),
            p.pos,
        ));
    }
    Ok(expr)
}

pub fn extract_prefixes(expr: &XPathExpr) -> HashSet<String> {
    let mut set = HashSet::new();
    collect_prefixes(expr, &mut set);
    set
}

pub fn extract_functions(expr: &XPathExpr) -> Vec<QName> {
    let mut fns = Vec::new();
    collect_functions(expr, &mut fns);
    fns
}

pub const YANG_XPATH_FUNCTIONS: &[&str] = &[
    "last", "position", "count", "id", "local-name", "namespace-uri", "name",
    "string", "concat", "starts-with", "contains", "substring-before",
    "substring-after", "substring", "string-length", "normalize-space",
    "translate", "boolean", "not", "true", "false", "lang",
    "number", "sum", "floor", "ceiling", "round",
    "derived-from", "derived-from-or-self", "enum-value", "bit-is-set",
    "re-match", "deref", "current",
];

pub fn check_function_whitelist(expr: &XPathExpr) -> Result<(), QName> {
    let fns = extract_functions(expr);
    let allowed: HashSet<&str> = YANG_XPATH_FUNCTIONS.iter().copied().collect();
    for f in fns {
        if f.prefix.is_some() { continue; }
        if !allowed.contains(f.local.as_str()) {
            return Err(f);
        }
    }
    Ok(())
}

// ── Collectors ────────────────────────────────────────────────────────────────

fn collect_prefixes(expr: &XPathExpr, out: &mut HashSet<String>) {
    match expr {
        XPathExpr::Path(lp) => {
            for step in &lp.steps {
                match &step.node_test {
                    NodeTest::Name(qn) => { if let Some(p) = &qn.prefix { out.insert(p.clone()); } }
                    NodeTest::PrefixWildcard(p) => { out.insert(p.clone()); }
                    _ => {}
                }
                for pred in &step.predicates { collect_prefixes(pred, out); }
            }
        }
        XPathExpr::Variable(qn) => { if let Some(p) = &qn.prefix { out.insert(p.clone()); } }
        XPathExpr::FunctionCall { name, args } => {
            if let Some(p) = &name.prefix { out.insert(p.clone()); }
            for a in args { collect_prefixes(a, out); }
        }
        XPathExpr::Filter(e, preds) => {
            collect_prefixes(e, out);
            for p in preds { collect_prefixes(p, out); }
        }
        XPathExpr::Union(a, b) | XPathExpr::Or(a, b) | XPathExpr::And(a, b)
        | XPathExpr::Eq(a, b) | XPathExpr::Ne(a, b)
        | XPathExpr::Lt(a, b) | XPathExpr::Gt(a, b)
        | XPathExpr::Le(a, b) | XPathExpr::Ge(a, b)
        | XPathExpr::Add(a, b) | XPathExpr::Sub(a, b)
        | XPathExpr::Mul(a, b) | XPathExpr::Div(a, b)
        | XPathExpr::Mod(a, b) => { collect_prefixes(a, out); collect_prefixes(b, out); }
        XPathExpr::Neg(e) => collect_prefixes(e, out),
        XPathExpr::String(_) | XPathExpr::Number(_) => {}
    }
}

fn collect_functions(expr: &XPathExpr, out: &mut Vec<QName>) {
    match expr {
        XPathExpr::FunctionCall { name, args } => {
            out.push(name.clone());
            for a in args { collect_functions(a, out); }
        }
        XPathExpr::Path(lp) => {
            for step in &lp.steps {
                for pred in &step.predicates { collect_functions(pred, out); }
            }
        }
        XPathExpr::Filter(e, preds) => {
            collect_functions(e, out);
            for p in preds { collect_functions(p, out); }
        }
        XPathExpr::Union(a, b) | XPathExpr::Or(a, b) | XPathExpr::And(a, b)
        | XPathExpr::Eq(a, b) | XPathExpr::Ne(a, b)
        | XPathExpr::Lt(a, b) | XPathExpr::Gt(a, b)
        | XPathExpr::Le(a, b) | XPathExpr::Ge(a, b)
        | XPathExpr::Add(a, b) | XPathExpr::Sub(a, b)
        | XPathExpr::Mul(a, b) | XPathExpr::Div(a, b)
        | XPathExpr::Mod(a, b) => { collect_functions(a, out); collect_functions(b, out); }
        XPathExpr::Neg(e) => collect_functions(e, out),
        XPathExpr::Variable(_) | XPathExpr::String(_) | XPathExpr::Number(_) => {}
    }
}

// ── Lexer tokens ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Slash, SlashSlash, Dot, DotDot, At,
    LParen, RParen, LBracket, RBracket,
    Comma, Pipe, Plus, Minus, Star,
    Dollar, Eq, Ne, Lt, Gt, Le, Ge,
    StringLit(String),
    NumberLit(f64),
    Name(String),
    Eof,
}

// ── Parser ────────────────────────────────────────────────────────────────────

struct XParser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> XParser<'a> {
    fn new(input: &'a str) -> Self { XParser { input, pos: 0 } }

    fn peek_byte(&self) -> Option<u8> { self.input.as_bytes().get(self.pos).copied() }

    fn skip_ws(&mut self) {
        while matches!(self.peek_byte(), Some(b' '|b'\t'|b'\r'|b'\n')) { self.pos += 1; }
    }

    fn err(&self, msg: impl Into<String>) -> XPathError { XPathError::new(msg, self.pos) }

    // ── Tokenizer ─────────────────────────────────────────────────────────────

    fn peek_tok(&mut self) -> Tok {
        let s = self.pos; let t = self.next_tok(); self.pos = s; t
    }

    fn next_tok(&mut self) -> Tok {
        self.skip_ws();
        let b = match self.peek_byte() { None => return Tok::Eof, Some(b) => b };
        match b {
            b'/' => {
                self.pos += 1;
                if self.peek_byte() == Some(b'/') { self.pos += 1; Tok::SlashSlash } else { Tok::Slash }
            }
            b'.' => {
                self.pos += 1;
                if self.peek_byte() == Some(b'.') { self.pos += 1; Tok::DotDot }
                else if matches!(self.peek_byte(), Some(b'0'..=b'9')) {
                    let start = self.pos - 1;
                    while matches!(self.peek_byte(), Some(b'0'..=b'9')) { self.pos += 1; }
                    Tok::NumberLit(self.input[start..self.pos].parse().unwrap_or(0.0))
                } else { Tok::Dot }
            }
            b'@' => { self.pos += 1; Tok::At }
            b'(' => { self.pos += 1; Tok::LParen }
            b')' => { self.pos += 1; Tok::RParen }
            b'[' => { self.pos += 1; Tok::LBracket }
            b']' => { self.pos += 1; Tok::RBracket }
            b',' => { self.pos += 1; Tok::Comma }
            b'|' => { self.pos += 1; Tok::Pipe }
            b'+' => { self.pos += 1; Tok::Plus }
            b'-' => { self.pos += 1; Tok::Minus }
            b'*' => { self.pos += 1; Tok::Star }
            b'$' => { self.pos += 1; Tok::Dollar }
            b'=' => { self.pos += 1; Tok::Eq }
            b'!' => {
                self.pos += 1;
                if self.peek_byte() == Some(b'=') { self.pos += 1; Tok::Ne } else { Tok::Eof }
            }
            b'<' => {
                self.pos += 1;
                if self.peek_byte() == Some(b'=') { self.pos += 1; Tok::Le } else { Tok::Lt }
            }
            b'>' => {
                self.pos += 1;
                if self.peek_byte() == Some(b'=') { self.pos += 1; Tok::Ge } else { Tok::Gt }
            }
            b'\'' | b'"' => {
                let q = b; self.pos += 1; let start = self.pos;
                while self.peek_byte().map(|c| c != q).unwrap_or(false) { self.pos += 1; }
                let s = self.input[start..self.pos].to_string();
                if self.peek_byte() == Some(q) { self.pos += 1; }
                Tok::StringLit(s)
            }
            b'0'..=b'9' => {
                let start = self.pos;
                while matches!(self.peek_byte(), Some(b'0'..=b'9')) { self.pos += 1; }
                if self.peek_byte() == Some(b'.') {
                    self.pos += 1;
                    while matches!(self.peek_byte(), Some(b'0'..=b'9')) { self.pos += 1; }
                }
                Tok::NumberLit(self.input[start..self.pos].parse().unwrap_or(0.0))
            }
            _ if is_name_start(b) => {
                let start = self.pos;
                while matches!(self.peek_byte(), Some(c) if is_name_cont(c)) { self.pos += 1; }
                Tok::Name(self.input[start..self.pos].to_string())
            }
            _ => { self.pos += 1; Tok::Eof }
        }
    }

    // Check if next non-ws char is ':'  but NOT '::'
    fn peek_is_colon_not_dcolon(&self) -> bool {
        let mut i = self.pos;
        while matches!(self.input.as_bytes().get(i), Some(b' '|b'\t'|b'\r'|b'\n')) { i += 1; }
        matches!(self.input.as_bytes().get(i), Some(b':'))
            && !matches!(self.input.as_bytes().get(i+1), Some(b':'))
    }

    // ── Grammar ───────────────────────────────────────────────────────────────

    pub fn parse_expr(&mut self) -> Result<XPathExpr, XPathError> {
        self.parse_or_expr()
    }

    fn parse_or_expr(&mut self) -> Result<XPathExpr, XPathError> {
        let mut lhs = self.parse_and_expr()?;
        loop {
            let s = self.pos;
            if matches!(self.peek_tok(), Tok::Name(ref n) if n == "or") {
                self.next_tok();
                lhs = XPathExpr::Or(Box::new(lhs), Box::new(self.parse_and_expr()?));
            } else { self.pos = s; break; }
        }
        Ok(lhs)
    }

    fn parse_and_expr(&mut self) -> Result<XPathExpr, XPathError> {
        let mut lhs = self.parse_equality_expr()?;
        loop {
            let s = self.pos;
            if matches!(self.peek_tok(), Tok::Name(ref n) if n == "and") {
                self.next_tok();
                lhs = XPathExpr::And(Box::new(lhs), Box::new(self.parse_equality_expr()?));
            } else { self.pos = s; break; }
        }
        Ok(lhs)
    }

    fn parse_equality_expr(&mut self) -> Result<XPathExpr, XPathError> {
        let mut lhs = self.parse_relational_expr()?;
        loop {
            let s = self.pos;
            match self.peek_tok() {
                Tok::Eq => { self.next_tok(); lhs = XPathExpr::Eq(Box::new(lhs), Box::new(self.parse_relational_expr()?)); }
                Tok::Ne => { self.next_tok(); lhs = XPathExpr::Ne(Box::new(lhs), Box::new(self.parse_relational_expr()?)); }
                _ => { self.pos = s; break; }
            }
        }
        Ok(lhs)
    }

    fn parse_relational_expr(&mut self) -> Result<XPathExpr, XPathError> {
        let mut lhs = self.parse_additive_expr()?;
        loop {
            let s = self.pos;
            match self.peek_tok() {
                Tok::Lt => { self.next_tok(); lhs = XPathExpr::Lt(Box::new(lhs), Box::new(self.parse_additive_expr()?)); }
                Tok::Gt => { self.next_tok(); lhs = XPathExpr::Gt(Box::new(lhs), Box::new(self.parse_additive_expr()?)); }
                Tok::Le => { self.next_tok(); lhs = XPathExpr::Le(Box::new(lhs), Box::new(self.parse_additive_expr()?)); }
                Tok::Ge => { self.next_tok(); lhs = XPathExpr::Ge(Box::new(lhs), Box::new(self.parse_additive_expr()?)); }
                _ => { self.pos = s; break; }
            }
        }
        Ok(lhs)
    }

    fn parse_additive_expr(&mut self) -> Result<XPathExpr, XPathError> {
        let mut lhs = self.parse_multiplicative_expr()?;
        loop {
            let s = self.pos;
            match self.peek_tok() {
                Tok::Plus => { self.next_tok(); lhs = XPathExpr::Add(Box::new(lhs), Box::new(self.parse_multiplicative_expr()?)); }
                Tok::Minus => { self.next_tok(); lhs = XPathExpr::Sub(Box::new(lhs), Box::new(self.parse_multiplicative_expr()?)); }
                _ => { self.pos = s; break; }
            }
        }
        Ok(lhs)
    }

    fn parse_multiplicative_expr(&mut self) -> Result<XPathExpr, XPathError> {
        let mut lhs = self.parse_unary_expr()?;
        loop {
            let s = self.pos;
            match self.peek_tok() {
                Tok::Star => { self.next_tok(); lhs = XPathExpr::Mul(Box::new(lhs), Box::new(self.parse_unary_expr()?)); }
                Tok::Name(ref n) if n == "div" => { self.next_tok(); lhs = XPathExpr::Div(Box::new(lhs), Box::new(self.parse_unary_expr()?)); }
                Tok::Name(ref n) if n == "mod" => { self.next_tok(); lhs = XPathExpr::Mod(Box::new(lhs), Box::new(self.parse_unary_expr()?)); }
                _ => { self.pos = s; break; }
            }
        }
        Ok(lhs)
    }

    fn parse_unary_expr(&mut self) -> Result<XPathExpr, XPathError> {
        let s = self.pos;
        if let Tok::Minus = self.peek_tok() {
            self.next_tok();
            return Ok(XPathExpr::Neg(Box::new(self.parse_unary_expr()?)));
        }
        self.pos = s;
        self.parse_union_expr()
    }

    fn parse_union_expr(&mut self) -> Result<XPathExpr, XPathError> {
        let mut lhs = self.parse_path_expr()?;
        loop {
            let s = self.pos;
            if let Tok::Pipe = self.peek_tok() {
                self.next_tok();
                lhs = XPathExpr::Union(Box::new(lhs), Box::new(self.parse_path_expr()?));
            } else { self.pos = s; break; }
        }
        Ok(lhs)
    }

    fn parse_path_expr(&mut self) -> Result<XPathExpr, XPathError> {
        let s = self.pos;

        match self.peek_tok() {
            Tok::Slash | Tok::SlashSlash => return self.parse_absolute_path(),
            Tok::Dot | Tok::DotDot | Tok::At => return self.parse_relative_path(),
            Tok::Star => return self.parse_relative_path(),
            _ => {}
        }
        self.pos = s;

        // Name-based disambiguation
        if let Tok::Name(_) = self.peek_tok() {
            let s2 = self.pos;
            self.next_tok(); // consume name
            self.skip_ws();
            let after = self.peek_byte();
            self.pos = s2;

            if after == Some(b'(') {
                // function call → FilterExpr
                let fe = self.parse_filter_expr()?;
                return self.maybe_extend_path(fe);
            }
            // axis:: or name step
            return self.parse_relative_path();
        }

        // Primary expression (literal, $var, parens)
        let s3 = self.pos;
        if matches!(self.peek_tok(), Tok::Dollar | Tok::LParen | Tok::StringLit(_) | Tok::NumberLit(_)) {
            self.pos = s3;
            let fe = self.parse_filter_expr()?;
            return self.maybe_extend_path(fe);
        }
        self.pos = s3;
        Err(self.err("expected expression"))
    }

    fn maybe_extend_path(&mut self, base: XPathExpr) -> Result<XPathExpr, XPathError> {
        let s = self.pos;
        match self.peek_tok() {
            Tok::Slash | Tok::SlashSlash => {
                // base / relpath  — represent as Filter with step expressions
                let is_dslash = matches!(self.next_tok(), Tok::SlashSlash);
                let mut extra: Vec<XPathExpr> = Vec::new();
                if is_dslash {
                    extra.push(XPathExpr::Path(LocationPath {
                        absolute: false,
                        steps: vec![Step { axis: Axis::DescendantOrSelf, node_test: NodeTest::Node, predicates: vec![] }],
                    }));
                }
                let first_step = self.parse_step()?;
                extra.push(XPathExpr::Path(LocationPath { absolute: false, steps: vec![first_step] }));

                loop {
                    let s2 = self.pos;
                    match self.peek_tok() {
                        Tok::Slash => {
                            self.next_tok();
                            extra.push(XPathExpr::Path(LocationPath { absolute: false, steps: vec![self.parse_step()?] }));
                        }
                        Tok::SlashSlash => {
                            self.next_tok();
                            extra.push(XPathExpr::Path(LocationPath {
                                absolute: false,
                                steps: vec![Step { axis: Axis::DescendantOrSelf, node_test: NodeTest::Node, predicates: vec![] }],
                            }));
                            extra.push(XPathExpr::Path(LocationPath { absolute: false, steps: vec![self.parse_step()?] }));
                        }
                        _ => { self.pos = s2; break; }
                    }
                }
                Ok(XPathExpr::Filter(Box::new(base), extra))
            }
            _ => { self.pos = s; Ok(base) }
        }
    }

    fn parse_filter_expr(&mut self) -> Result<XPathExpr, XPathError> {
        let primary = self.parse_primary()?;
        let mut preds = Vec::new();
        loop {
            let s = self.pos;
            if let Tok::LBracket = self.peek_tok() {
                self.next_tok();
                preds.push(self.parse_expr()?);
                self.skip_ws();
                if !matches!(self.next_tok(), Tok::RBracket) {
                    return Err(self.err("expected ']'"));
                }
            } else { self.pos = s; break; }
        }
        if preds.is_empty() { Ok(primary) } else { Ok(XPathExpr::Filter(Box::new(primary), preds)) }
    }

    fn parse_primary(&mut self) -> Result<XPathExpr, XPathError> {
        match self.next_tok() {
            Tok::Dollar => {
                if let Tok::Name(n) = self.next_tok() {
                    let qn = if self.peek_is_colon_not_dcolon() {
                        self.skip_ws(); self.pos += 1; // skip ':'
                        if let Tok::Name(local) = self.next_tok() {
                            QName::qualified(n, local)
                        } else { return Err(self.err("expected name after ':'"))}
                    } else { QName::bare(n) };
                    Ok(XPathExpr::Variable(qn))
                } else { Err(self.err("expected name after '$'")) }
            }
            Tok::LParen => {
                let e = self.parse_expr()?;
                self.skip_ws();
                if !matches!(self.next_tok(), Tok::RParen) { return Err(self.err("expected ')'")) }
                Ok(e)
            }
            Tok::StringLit(s) => Ok(XPathExpr::String(s)),
            Tok::NumberLit(n) => Ok(XPathExpr::Number(n)),
            Tok::Name(n) => {
                // function call: name [':' local] '('
                let fname = if self.peek_is_colon_not_dcolon() {
                    self.skip_ws(); self.pos += 1;
                    if let Tok::Name(local) = self.next_tok() {
                        QName::qualified(n, local)
                    } else { return Err(self.err("expected name")) }
                } else { QName::bare(n) };
                self.skip_ws();
                if !matches!(self.next_tok(), Tok::LParen) { return Err(self.err("expected '('")); }
                let mut args = Vec::new();
                self.skip_ws();
                if !matches!(self.peek_tok(), Tok::RParen) {
                    args.push(self.parse_expr()?);
                    loop {
                        let s = self.pos;
                        match self.next_tok() {
                            Tok::Comma => args.push(self.parse_expr()?),
                            Tok::RParen => break,
                            _ => { self.pos = s; return Err(self.err("expected ',' or ')'")); }
                        }
                    }
                } else { self.next_tok(); /* RParen */ }
                Ok(XPathExpr::FunctionCall { name: fname, args })
            }
            tok => Err(self.err(format!("unexpected {:?} in primary expression", tok))),
        }
    }

    fn parse_absolute_path(&mut self) -> Result<XPathExpr, XPathError> {
        let is_dslash = matches!(self.next_tok(), Tok::SlashSlash);
        let mut steps = Vec::new();
        if is_dslash {
            steps.push(Step { axis: Axis::DescendantOrSelf, node_test: NodeTest::Node, predicates: vec![] });
            steps.push(self.parse_step()?);
        } else {
            // single '/' — may or may not be followed by a step
            let s = self.pos;
            match self.peek_tok() {
                Tok::Eof | Tok::RBracket | Tok::RParen | Tok::Pipe
                | Tok::Comma | Tok::Eq | Tok::Ne | Tok::Lt | Tok::Gt
                | Tok::Le | Tok::Ge | Tok::Plus | Tok::Minus => { self.pos = s; }
                Tok::Name(ref n) if matches!(n.as_str(), "or"|"and"|"mod"|"div") => { self.pos = s; }
                _ => { self.pos = s; steps.push(self.parse_step()?); }
            }
        }
        loop {
            let s = self.pos;
            match self.peek_tok() {
                Tok::Slash => { self.next_tok(); steps.push(self.parse_step()?); }
                Tok::SlashSlash => {
                    self.next_tok();
                    steps.push(Step { axis: Axis::DescendantOrSelf, node_test: NodeTest::Node, predicates: vec![] });
                    steps.push(self.parse_step()?);
                }
                _ => { self.pos = s; break; }
            }
        }
        Ok(XPathExpr::Path(LocationPath { absolute: true, steps }))
    }

    fn parse_relative_path(&mut self) -> Result<XPathExpr, XPathError> {
        let mut steps = vec![self.parse_step()?];
        loop {
            let s = self.pos;
            match self.peek_tok() {
                Tok::Slash => { self.next_tok(); steps.push(self.parse_step()?); }
                Tok::SlashSlash => {
                    self.next_tok();
                    steps.push(Step { axis: Axis::DescendantOrSelf, node_test: NodeTest::Node, predicates: vec![] });
                    steps.push(self.parse_step()?);
                }
                _ => { self.pos = s; break; }
            }
        }
        Ok(XPathExpr::Path(LocationPath { absolute: false, steps }))
    }

    fn parse_step(&mut self) -> Result<Step, XPathError> {
        let s = self.pos;
        match self.peek_tok() {
            Tok::Dot => { self.next_tok(); return Ok(Step { axis: Axis::Self_, node_test: NodeTest::Node, predicates: vec![] }); }
            Tok::DotDot => { self.next_tok(); return Ok(Step { axis: Axis::Parent, node_test: NodeTest::Node, predicates: vec![] }); }
            Tok::At => {
                self.next_tok();
                let nt = self.parse_name_or_node_test()?;
                return Ok(Step { axis: Axis::Attribute, node_test: nt, predicates: self.parse_predicates()? });
            }
            Tok::Star => {
                self.next_tok();
                return Ok(Step { axis: Axis::Child, node_test: NodeTest::Wildcard, predicates: self.parse_predicates()? });
            }
            _ => {}
        }
        self.pos = s;

        if let Tok::Name(n) = self.next_tok() {
            self.skip_ws();
            // axis:: ?
            if self.input.as_bytes().get(self.pos) == Some(&b':')
               && self.input.as_bytes().get(self.pos + 1) == Some(&b':') {
                if let Some(axis) = Axis::from_str(&n) {
                    self.pos += 2;
                    let nt = self.parse_name_or_node_test()?;
                    return Ok(Step { axis, node_test: nt, predicates: self.parse_predicates()? });
                }
                // unknown axis name — fall through as name test
                self.pos = s;
                if let Tok::Name(n2) = self.next_tok() {
                    let nt = self.finish_name_test(n2)?;
                    return Ok(Step { axis: Axis::Child, node_test: nt, predicates: self.parse_predicates()? });
                }
            }
            let nt = self.finish_name_test(n)?;
            return Ok(Step { axis: Axis::Child, node_test: nt, predicates: self.parse_predicates()? });
        }

        self.pos = s;
        Err(self.err("expected step"))
    }

    /// Given a name token already consumed, parse the rest of a NameTest or NodeType.
    fn finish_name_test(&mut self, name: String) -> Result<NodeTest, XPathError> {
        self.skip_ws();
        // node-type functions
        if self.peek_byte() == Some(b'(') {
            return match name.as_str() {
                "node" => { self.pos += 1; self.expect_rparen()?; Ok(NodeTest::Node) }
                "text" => { self.pos += 1; self.expect_rparen()?; Ok(NodeTest::Text) }
                "comment" => { self.pos += 1; self.expect_rparen()?; Ok(NodeTest::Comment) }
                "processing-instruction" => {
                    self.pos += 1; self.skip_ws();
                    let lit = if matches!(self.peek_byte(), Some(b'\'')|Some(b'"')) {
                        if let Tok::StringLit(s) = self.next_tok() { Some(s) } else { None }
                    } else { None };
                    self.skip_ws(); self.expect_rparen()?;
                    Ok(NodeTest::Pi(lit))
                }
                _ => Err(self.err(format!("'{}' is not a node type", name))),
            };
        }
        // prefix:local or prefix:*
        if self.peek_is_colon_not_dcolon() {
            self.skip_ws(); self.pos += 1; self.skip_ws(); // skip ':'
            if self.peek_byte() == Some(b'*') {
                self.pos += 1;
                return Ok(NodeTest::PrefixWildcard(name));
            }
            if let Tok::Name(local) = self.next_tok() {
                return Ok(NodeTest::Name(QName::qualified(name, local)));
            }
            return Err(self.err("expected local name"));
        }
        Ok(NodeTest::Name(QName::bare(name)))
    }

    fn parse_name_or_node_test(&mut self) -> Result<NodeTest, XPathError> {
        match self.next_tok() {
            Tok::Star => Ok(NodeTest::Wildcard),
            Tok::Name(n) => self.finish_name_test(n),
            _ => Err(self.err("expected name test")),
        }
    }

    fn parse_predicates(&mut self) -> Result<Vec<XPathExpr>, XPathError> {
        let mut preds = Vec::new();
        loop {
            let s = self.pos;
            if let Tok::LBracket = self.peek_tok() {
                self.next_tok();
                preds.push(self.parse_expr()?);
                self.skip_ws();
                if !matches!(self.next_tok(), Tok::RBracket) { return Err(self.err("expected ']'")); }
            } else { self.pos = s; break; }
        }
        Ok(preds)
    }

    fn expect_rparen(&mut self) -> Result<(), XPathError> {
        self.skip_ws();
        if matches!(self.next_tok(), Tok::RParen) { Ok(()) } else { Err(self.err("expected ')'")) }
    }
}

// ── Character helpers ─────────────────────────────────────────────────────────

fn is_name_start(b: u8) -> bool {
    matches!(b, b'A'..=b'Z'|b'a'..=b'z'|b'_') || b > 127
}

fn is_name_cont(b: u8) -> bool {
    matches!(b, b'A'..=b'Z'|b'a'..=b'z'|b'0'..=b'9'|b'_'|b'-'|b'.') || b > 127
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_path() {
        let expr = parse_xpath("/prefix:a/prefix:b").unwrap();
        let prefixes = extract_prefixes(&expr);
        assert!(prefixes.contains("prefix"), "{:?}", prefixes);
    }

    #[test]
    fn parse_function_call() {
        let expr = parse_xpath("count(../sibling)").unwrap();
        let fns = extract_functions(&expr);
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].local, "count");
    }

    #[test]
    fn whitelist_ok() {
        let expr = parse_xpath("derived-from(../type, 'base:my-identity')").unwrap();
        assert!(check_function_whitelist(&expr).is_ok());
    }

    #[test]
    fn whitelist_reject_unknown() {
        let expr = parse_xpath("unknown-func(.)").unwrap();
        assert!(check_function_whitelist(&expr).is_err());
    }

    #[test]
    fn parse_predicate() {
        let expr = parse_xpath("/a:list[a:key = 'val']").unwrap();
        match &expr {
            XPathExpr::Path(lp) => {
                assert!(lp.absolute);
                assert!(!lp.steps.is_empty());
                assert_eq!(lp.steps[0].predicates.len(), 1);
            }
            _ => panic!("expected path, got {:?}", expr),
        }
    }

    #[test]
    fn parse_or_and() {
        let expr = parse_xpath("a = 1 or b = 2 and c = 3").unwrap();
        assert!(matches!(expr, XPathExpr::Or(_, _)));
    }

    #[test]
    fn parse_current() {
        let expr = parse_xpath("current()/../sibling").unwrap();
        let fns = extract_functions(&expr);
        assert!(fns.iter().any(|f| f.local == "current"), "{:?}", fns);
    }

    #[test]
    fn parse_string_lit() {
        let expr = parse_xpath("'hello world'").unwrap();
        assert_eq!(expr, XPathExpr::String("hello world".to_string()));
    }

    #[test]
    fn parse_number() {
        let expr = parse_xpath("42").unwrap();
        assert_eq!(expr, XPathExpr::Number(42.0));
    }

    #[test]
    fn parse_abbreviated_ancestor() {
        let expr = parse_xpath("../../leaf").unwrap();
        match &expr {
            XPathExpr::Path(lp) => {
                assert!(!lp.absolute);
                assert_eq!(lp.steps.len(), 3);
            }
            _ => panic!("expected path"),
        }
    }

    #[test]
    fn parse_axis_specifier() {
        let expr = parse_xpath("descendant-or-self::node()").unwrap();
        match &expr {
            XPathExpr::Path(lp) => {
                assert_eq!(lp.steps[0].axis, Axis::DescendantOrSelf);
                assert_eq!(lp.steps[0].node_test, NodeTest::Node);
            }
            _ => panic!("expected path"),
        }
    }
}
