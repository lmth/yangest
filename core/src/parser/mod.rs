// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! YANG text-format tokenizer and recursive-descent statement parser.
//!
//! Produces a `Vec<Stmt>` (the top-level statements of a .yang file) and a
//! list of `YError`s, following the error-accumulation model: parsing continues
//! past recoverable errors where possible.
//!
//! Grammar reference: RFC 7950 §6.

use std::sync::Arc;

use crate::ast::{
    BuiltInKeyword, ErrorCode, Keyword, Level, Pos, Stmt, YError,
};

// ── Public entry point ────────────────────────────────────────────────────────

/// Parse a YANG text file.
///
/// `source`  — full text of the .yang file  
/// `file`    — file path / name (used in `Pos` for error reporting)
///
/// Returns `(stmts, errors)`.  Errors are non-fatal by default; callers
/// decide whether to abort on any `Level::Error`.
pub fn parse_yang(source: &str, file: Arc<str>) -> (Vec<Stmt>, Vec<YError>) {
    let mut p = Parser::new(source, file);
    p.parse_stmts_top();
    (p.stmts, p.errors)
}

// ── Token types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Token<'a> {
    /// An unquoted identifier or keyword token.
    Identifier(&'a str),
    /// A string argument (single- or double-quoted, possibly concatenated).
    Quoted(String),
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `;`
    Semi,
    /// End of input.
    Eof,
}

// ── Lexer / parser state ──────────────────────────────────────────────────────

struct Parser<'a> {
    src: &'a str,
    /// Current byte offset into `src`.
    pos: usize,
    /// Current 1-based line number.
    line: u32,
    file: Arc<str>,

    stmts: Vec<Stmt>,
    errors: Vec<YError>,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str, file: Arc<str>) -> Self {
        Parser { src, pos: 0, line: 1, file, stmts: Vec::new(), errors: Vec::new() }
    }

    fn current_pos(&self) -> Pos {
        Pos::FilePos { file: Arc::clone(&self.file), line: self.line }
    }

    fn emit_error(&mut self, pos: Pos, code: ErrorCode, message: impl Into<String>) {
        self.errors.push(YError { level: Level::Error, pos, code, message: message.into() });
    }

    fn emit_warning(&mut self, pos: Pos, code: ErrorCode, message: impl Into<String>) {
        self.errors.push(YError { level: Level::Warning, pos, code, message: message.into() });
    }

    // ── Character helpers ─────────────────────────────────────────────────────

    fn peek(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos + 1).copied()
    }

    fn advance(&mut self) -> u8 {
        let b = self.src.as_bytes()[self.pos];
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
        }
        b
    }

    /// Advance past the current UTF-8 character and return it as a `char`.
    /// For ASCII bytes (< 0x80) this advances 1 byte; for multi-byte sequences
    /// it advances the full character length.
    fn advance_char(&mut self) -> char {
        let s = &self.src[self.pos..];
        let ch = s.chars().next().unwrap_or('\0');
        let len = ch.len_utf8();
        // Track newlines within the advanced bytes
        for &b in &self.src.as_bytes()[self.pos..self.pos + len] {
            if b == b'\n' {
                self.line += 1;
            }
        }
        self.pos += len;
        ch
    }

    #[allow(dead_code)]
    fn remaining(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            // Skip whitespace
            while matches!(self.peek(), Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n')) {
                self.advance();
            }
            // Check for comments
            if self.peek() == Some(b'/') {
                match self.peek2() {
                    Some(b'/') => {
                        // Single-line comment: skip to end of line
                        while !matches!(self.peek(), None | Some(b'\n')) {
                            self.advance();
                        }
                        continue;
                    }
                    Some(b'*') => {
                        // Block comment: skip until */
                        let start_line = self.line;
                        self.advance(); // /
                        self.advance(); // *
                        loop {
                            match self.peek() {
                                None => {
                                    let pos = Pos::FilePos {
                                        file: Arc::clone(&self.file),
                                        line: start_line,
                                    };
                                    self.emit_error(
                                        pos,
                                        ErrorCode::ParseUnterminatedString,
                                        "unterminated block comment",
                                    );
                                    return;
                                }
                                Some(b'*') if self.peek2() == Some(b'/') => {
                                    self.advance(); // *
                                    self.advance(); // /
                                    break;
                                }
                                _ => {
                                    self.advance();
                                }
                            }
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            break;
        }
    }

    // ── Quoted string reader ──────────────────────────────────────────────────

    /// Reads a single- or double-quoted string, handling YANG escape sequences.
    /// Called after the opening quote has been identified but not consumed.
    fn read_quoted_string(&mut self) -> String {
        let col = self.current_col(); // column of the opening quote
        let quote = self.advance(); // consume opening quote
        let mut buf = String::new();

        if quote == b'\'' {
            // Single-quoted: no escape sequences (RFC 7950 §6.1.3)
            loop {
                match self.peek() {
                    None => {
                        let pos = self.current_pos();
                        self.emit_error(
                            pos,
                            ErrorCode::ParseUnterminatedString,
                            "unterminated single-quoted string",
                        );
                        break;
                    }
                    Some(b'\'') => {
                        self.advance();
                        break;
                    }
                    _ => {
                        buf.push(self.advance_char());
                    }
                }
            }
        } else {
            // Double-quoted: handle escapes (RFC 7950 §6.1.3)
            loop {
                match self.peek() {
                    None => {
                        let pos = self.current_pos();
                        self.emit_error(
                            pos,
                            ErrorCode::ParseUnterminatedString,
                            "unterminated double-quoted string",
                        );
                        break;
                    }
                    Some(b'"') => {
                        self.advance();
                        break;
                    }
                    Some(b'\\') => {
                        self.advance(); // consume backslash
                        match self.peek() {
                            Some(b'n') => { self.advance(); buf.push('\n'); }
                            Some(b't') => { self.advance(); buf.push('\t'); }
                            Some(b'"') => { self.advance(); buf.push('"'); }
                            Some(b'\\') => { self.advance(); buf.push('\\'); }
                            _ => {
                                // Invalid escape — pass through with warning
                                let pos = self.current_pos();
                                self.emit_warning(
                                    pos,
                                    ErrorCode::ParseUnexpectedChar,
                                    "unknown escape sequence in double-quoted string",
                                );
                                if let Some(_) = self.peek() {
                                    buf.push(self.advance_char());
                                }
                            }
                        }
                    }
                    _ => {
                        buf.push(self.advance_char());
                    }
                }
            }
        }

        // Apply multi-line indentation stripping for double-quoted strings
        if quote == b'"' {
            buf = Self::normalize_double_quoted(&buf, col);
        }
        buf
    }

    /// Strip common leading whitespace from a multi-line double-quoted string
    /// per RFC 7950 §6.1.3 (trim leading spaces up to the column of the
    /// opening quote on continuation lines).  Additionally, trailing whitespace
    /// is stripped from all interior lines (before a newline) to match confdc's
    /// normalisation behaviour; trailing whitespace on the final line is
    /// preserved.
    fn normalize_double_quoted(s: &str, indent: usize) -> String {
        let lines: Vec<&str> = s.split('\n').collect();
        if lines.len() <= 1 {
            return s.to_string();
        }
        let last_idx = lines.len() - 1;
        let result: Vec<String> = lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let mut stripped = *line;
                if i > 0 {
                    // Remove up to `indent` leading spaces/tabs on continuation lines.
                    let mut removed = 0;
                    while removed < indent {
                        if stripped.starts_with(' ') {
                            stripped = &stripped[1..];
                            removed += 1;
                        } else if stripped.starts_with('\t') {
                            stripped = &stripped[1..];
                            removed += 1;
                        } else {
                            break;
                        }
                    }
                }
                // Strip trailing whitespace from all interior lines; preserve it on the last.
                if i < last_idx {
                    stripped.trim_end().to_string()
                } else {
                    stripped.to_string()
                }
            })
            .collect();
        result.join("\n")
    }

    // ── Tokenizer ─────────────────────────────────────────────────────────────

    /// Read the next token, skipping whitespace and comments.
    /// The token borrows from `self.src` where possible.
    fn next_token(&mut self) -> (Token<'a>, Pos) {
        self.skip_ws_and_comments();
        let pos = self.current_pos();

        match self.peek() {
            None => (Token::Eof, pos),
            Some(b'{') => { self.advance(); (Token::LBrace, pos) }
            Some(b'}') => { self.advance(); (Token::RBrace, pos) }
            Some(b';') => { self.advance(); (Token::Semi, pos) }

            Some(b'"') | Some(b'\'') => {
                // Handle concatenated strings: "foo" + "bar"
                let _col = self.current_col();
                let mut combined = self.read_quoted_string();
                // Apply double-quote normalization right after reading
                // (we track whether it was double-quoted by the first char)
                loop {
                    self.skip_ws_and_comments();
                    if self.peek() == Some(b'+') {
                        self.advance(); // consume +
                        self.skip_ws_and_comments();
                        match self.peek() {
                            Some(b'"') | Some(b'\'') => {
                                combined.push_str(&self.read_quoted_string());
                            }
                            _ => {
                                let pos2 = self.current_pos();
                                self.emit_error(
                                    pos2,
                                    ErrorCode::ParseUnexpectedChar,
                                    "expected string after '+' in string concatenation",
                                );
                                break;
                            }
                        }
                    } else {
                        break;
                    }
                }
                (Token::Quoted(combined), pos)
            }

            _ => {
                // Unquoted token: read until whitespace or { } ; "
                // Single-quote is NOT a terminator per RFC 7950 §6.1.3 —
                // it only opens a quoted string when it appears at the start
                // of an argument position (handled by the arm above).
                let start = self.pos;
                while matches!(
                    self.peek(),
                    Some(c) if !matches!(c, b' '|b'\t'|b'\r'|b'\n'|b'{'|b'}'|b';'|b'"')
                ) {
                    self.advance();
                }
                let tok = &self.src[start..self.pos];
                (Token::Identifier(tok), pos)
            }
        }
    }

    fn current_col(&self) -> usize {
        // Find last newline before self.pos
        let slice = &self.src[..self.pos];
        slice.rfind('\n').map(|i| self.pos - i).unwrap_or(self.pos + 1)
    }

    // ── Statement parsing ─────────────────────────────────────────────────────

    fn parse_stmts_top(&mut self) {
        let mut stmts = Vec::new();
        loop {
            self.skip_ws_and_comments();
            if self.peek().is_none() {
                break;
            }
            if let Some(stmt) = self.parse_stmt() {
                stmts.push(stmt);
            }
        }
        self.stmts = stmts;
    }

    fn parse_stmts_body(&mut self) -> Vec<Stmt> {
        let mut stmts = Vec::new();
        loop {
            self.skip_ws_and_comments();
            match self.peek() {
                None | Some(b'}') => break,
                _ => {
                    if let Some(stmt) = self.parse_stmt() {
                        stmts.push(stmt);
                    }
                }
            }
        }
        stmts
    }

    /// Parse a single statement: `keyword [argument] ( ';' | '{' stmts* '}' )`
    fn parse_stmt(&mut self) -> Option<Stmt> {
        self.skip_ws_and_comments();
        let (kw_tok, kw_pos) = self.next_token();

        let keyword = match kw_tok {
            Token::Eof => return None,
            Token::RBrace => {
                // Unexpected } — caller will handle
                return None;
            }
            Token::Identifier(s) => self.parse_keyword(s, &kw_pos),
            Token::Quoted(_) => {
                self.emit_error(
                    kw_pos,
                    ErrorCode::ParseUnexpectedChar,
                    "expected keyword, got quoted string",
                );
                return None;
            }
            tok => {
                self.emit_error(
                    kw_pos,
                    ErrorCode::ParseUnexpectedChar,
                    format!("unexpected token {:?}", tok),
                );
                return None;
            }
        };

        // Look for optional argument, then ; or {
        self.skip_ws_and_comments();
        let (arg, substmts) = match self.peek() {
            Some(b';') => {
                self.advance();
                (None, Vec::new())
            }
            Some(b'{') => {
                self.advance();
                let subs = self.parse_stmts_body();
                self.expect_rbrace(&kw_pos);
                (None, subs)
            }
            None => {
                self.emit_error(
                    kw_pos.clone(),
                    ErrorCode::ParseExpectedSemiOrBrace,
                    format!("unexpected end of file after keyword '{}'", keyword),
                );
                (None, Vec::new())
            }
            _ => {
                // Read argument
                let arg_val = self.read_argument();
                self.skip_ws_and_comments();
                match self.peek() {
                    Some(b';') => {
                        self.advance();
                        (Some(arg_val), Vec::new())
                    }
                    Some(b'{') => {
                        self.advance();
                        let subs = self.parse_stmts_body();
                        self.expect_rbrace(&kw_pos);
                        (Some(arg_val), subs)
                    }
                    _ => {
                        let pos2 = self.current_pos();
                        self.emit_error(
                            pos2,
                            ErrorCode::ParseExpectedSemiOrBrace,
                            format!("expected ';' or '{{' after argument of '{}'", keyword),
                        );
                        (Some(arg_val), Vec::new())
                    }
                }
            }
        };

        Some(Stmt::new(keyword, arg, kw_pos, substmts))
    }

    /// Parse keyword string into a `Keyword` value.
    fn parse_keyword(&self, s: &str, _pos: &Pos) -> Keyword {
        if let Some(colon) = s.find(':') {
            // Extension keyword  prefix:local
            let prefix = s[..colon].to_string();
            let name = s[colon + 1..].to_string();
            Keyword::ExtensionPrefixed { prefix, name }
        } else if let Some(kw) = BuiltInKeyword::from_str(s) {
            Keyword::BuiltIn(kw)
        } else {
            // Unknown unqualified keyword — still parse structurally, error at grammar check
            Keyword::ExtensionPrefixed { prefix: String::new(), name: s.to_string() }
        }
    }

    /// Read an argument token (quoted string or unquoted identifier).
    /// Handles YANG string concatenation with `+`.
    fn read_argument(&mut self) -> String {
        self.skip_ws_and_comments();
        match self.peek() {
            Some(b'"') | Some(b'\'') => {
                let mut combined = self.read_quoted_string();
                // Handle concatenation: "foo" + "bar"
                loop {
                    self.skip_ws_and_comments();
                    if self.peek() == Some(b'+') {
                        self.advance();
                        self.skip_ws_and_comments();
                        match self.peek() {
                            Some(b'"') | Some(b'\'') => {
                                combined.push_str(&self.read_quoted_string());
                            }
                            _ => {
                                let pos2 = self.current_pos();
                                self.emit_error(
                                    pos2,
                                    ErrorCode::ParseUnexpectedChar,
                                    "expected string after '+' in string concatenation",
                                );
                                break;
                            }
                        }
                    } else {
                        break;
                    }
                }
                combined
            }
            _ => {
                let start = self.pos;
                while matches!(
                    self.peek(),
                    Some(c) if !matches!(c, b' '|b'\t'|b'\r'|b'\n'|b'{'|b'}'|b';'|b'"')
                ) {
                    self.advance();
                }
                self.src[start..self.pos].to_string()
            }
        }
    }

    fn expect_rbrace(&mut self, _open_pos: &Pos) {
        self.skip_ws_and_comments();
        match self.peek() {
            Some(b'}') => { self.advance(); }
            _ => {
                let pos = self.current_pos();
                self.emit_error(
                    pos,
                    ErrorCode::ParseExpectedSemiOrBrace,
                    "expected '}' to close block",
                );
            }
        }
    }
}

// ── Grammar spec ──────────────────────────────────────────────────────────────

/// Occurrence constraint for a substatement in the grammar spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occurs {
    /// Exactly 1.
    One,
    /// 0 or 1.
    Optional,
    /// 0 or more.
    Many,
    /// 1 or more.
    AtLeastOne,
}

/// One row in the grammar spec table describing which substatements are allowed
/// under a given parent keyword, and their occurrence constraints.
pub struct SubstmtSpec {
    pub keyword: BuiltInKeyword,
    pub occurs: Occurs,
    /// Minimum YANG version: 1 = 1.0, 11 = 1.1
    pub min_version: u8,
}

impl SubstmtSpec {
    const fn new(keyword: BuiltInKeyword, occurs: Occurs) -> Self {
        SubstmtSpec { keyword, occurs, min_version: 1 }
    }
    const fn v11(keyword: BuiltInKeyword, occurs: Occurs) -> Self {
        SubstmtSpec { keyword, occurs, min_version: 11 }
    }
}

/// Argument type for a YANG keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgType {
    /// No argument.
    None,
    /// An identifier (identifier-ref or identifier).
    Identifier,
    /// A module name (identifier).
    ModuleName,
    /// A node-identifier or schema node id.
    NodeIdentifier,
    /// An absolute schema node id (starts with /).
    AbsoluteSchemaNodeId,
    /// A descendant schema node id.
    DescendantSchemaNodeId,
    /// An XPath expression.
    XPath,
    /// A URI string.
    Uri,
    /// A date string YYYY-MM-DD.
    Date,
    /// A free-form string.
    String,
    /// Exactly "true" or "false".
    Boolean,
    /// A non-negative integer or "unbounded".
    NonNegIntOrUnbounded,
    /// A non-negative integer.
    NonNegInt,
    /// An integer (may be negative).
    Int,
    /// A fraction-digits value (1..18).
    FractionDigits,
    /// "add", "replace", "delete", "not-supported".
    DeviateArg,
    /// "current", "deprecated", "obsolete".
    StatusArg,
    /// "system" or "user".
    OrderedByArg,
    /// "unbounded" or non-negative integer.
    MaxValue,
    /// Yang version string: "1" or "1.1".
    YangVersion,
    /// "not" | "and" | "or" and feature names (if-feature-expr).
    IfFeatureExpr,
    /// range/length expression.
    RangeArg,
    /// "none" (only for modifier statement).
    ModifierArg,
}

/// Full grammar entry for one keyword.
pub struct GrammarEntry {
    pub keyword: BuiltInKeyword,
    pub arg_type: ArgType,
    /// Minimum YANG version (1 or 11).
    pub min_version: u8,
    pub substmts: &'static [SubstmtSpec],
}

// Substatement spec arrays are defined as `const` slices so the grammar table
// can live in static storage.  Only a representative subset is listed here to
// keep the initial implementation manageable; the full table will be fleshed
// out iteratively as validation tests are added.

static MODULE_SUBSTMTS: &[SubstmtSpec] = &[
    SubstmtSpec::new(BuiltInKeyword::YangVersion, Occurs::Optional),
    SubstmtSpec::new(BuiltInKeyword::Namespace, Occurs::One),
    SubstmtSpec::new(BuiltInKeyword::Prefix, Occurs::One),
    SubstmtSpec::new(BuiltInKeyword::Import, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Include, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Organization, Occurs::Optional),
    SubstmtSpec::new(BuiltInKeyword::Contact, Occurs::Optional),
    SubstmtSpec::new(BuiltInKeyword::Description, Occurs::Optional),
    SubstmtSpec::new(BuiltInKeyword::Reference, Occurs::Optional),
    SubstmtSpec::new(BuiltInKeyword::Revision, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Extension, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Feature, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Identity, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Typedef, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Grouping, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Container, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Leaf, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::LeafList, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::List, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Choice, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::AnyXml, Occurs::Many),
    SubstmtSpec::v11(BuiltInKeyword::AnyData, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Uses, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Augment, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Rpc, Occurs::Many),
    SubstmtSpec::v11(BuiltInKeyword::Action, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Notification, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Deviation, Occurs::Many),
];

static IMPORT_SUBSTMTS: &[SubstmtSpec] = &[
    SubstmtSpec::new(BuiltInKeyword::Prefix, Occurs::One),
    SubstmtSpec::new(BuiltInKeyword::RevisionDate, Occurs::Optional),
    SubstmtSpec::new(BuiltInKeyword::Description, Occurs::Optional),
    SubstmtSpec::new(BuiltInKeyword::Reference, Occurs::Optional),
];

static LEAF_SUBSTMTS: &[SubstmtSpec] = &[
    SubstmtSpec::new(BuiltInKeyword::When, Occurs::Optional),
    SubstmtSpec::new(BuiltInKeyword::IfFeature, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Type, Occurs::One),
    SubstmtSpec::new(BuiltInKeyword::Units, Occurs::Optional),
    SubstmtSpec::new(BuiltInKeyword::Must, Occurs::Many),
    SubstmtSpec::new(BuiltInKeyword::Default, Occurs::Optional),
    SubstmtSpec::new(BuiltInKeyword::Config, Occurs::Optional),
    SubstmtSpec::new(BuiltInKeyword::Mandatory, Occurs::Optional),
    SubstmtSpec::new(BuiltInKeyword::Status, Occurs::Optional),
    SubstmtSpec::new(BuiltInKeyword::Description, Occurs::Optional),
    SubstmtSpec::new(BuiltInKeyword::Reference, Occurs::Optional),
];

// The public grammar table. Entries for all keywords will be added as the
// implementation progresses; having the table separate from the parser means
// grammar validation can be iterated without touching the tokenizer.
pub static GRAMMAR: &[GrammarEntry] = &[
    GrammarEntry {
        keyword: BuiltInKeyword::Module,
        arg_type: ArgType::ModuleName,
        min_version: 1,
        substmts: MODULE_SUBSTMTS,
    },
    GrammarEntry {
        keyword: BuiltInKeyword::Import,
        arg_type: ArgType::ModuleName,
        min_version: 1,
        substmts: IMPORT_SUBSTMTS,
    },
    GrammarEntry {
        keyword: BuiltInKeyword::Leaf,
        arg_type: ArgType::Identifier,
        min_version: 1,
        substmts: LEAF_SUBSTMTS,
    },
    // … additional entries to be added incrementally
];

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> Arc<str> {
        Arc::from("test.yang")
    }

    #[test]
    fn parse_simple_leaf() {
        let src = r#"
module foo {
  yang-version 1.1;
  namespace "urn:foo";
  prefix foo;
  leaf my-leaf {
    type string;
  }
}
"#;
        let (stmts, errors) = parse_yang(src, file());
        assert!(errors.is_empty(), "errors: {:?}", errors);
        assert_eq!(stmts.len(), 1);
        let m = &stmts[0];
        assert!(m.keyword.is_builtin(BuiltInKeyword::Module));
        assert_eq!(m.arg_str(), "foo");
        let leaf = m.get_substmt(BuiltInKeyword::Leaf).expect("leaf");
        assert_eq!(leaf.arg_str(), "my-leaf");
    }

    #[test]
    fn parse_quoted_concatenation() {
        let src = r#"description "hello " + "world";"#;
        let (stmts, errors) = parse_yang(src, file());
        assert!(errors.is_empty(), "errors: {:?}", errors);
        assert_eq!(stmts[0].arg_str(), "hello world");
    }

    #[test]
    fn parse_single_quoted() {
        let src = r#"description 'no escapes \n here';"#;
        let (stmts, errors) = parse_yang(src, file());
        assert!(errors.is_empty(), "errors: {:?}", errors);
        assert_eq!(stmts[0].arg_str(), r#"no escapes \n here"#);
    }

    #[test]
    fn parse_double_quoted_escape() {
        let src = r#"description "line1\nline2";"#;
        let (stmts, errors) = parse_yang(src, file());
        assert!(errors.is_empty(), "errors: {:?}", errors);
        assert_eq!(stmts[0].arg_str(), "line1\nline2");
    }

    #[test]
    fn parse_block_comment() {
        let src = r#"/* a comment */ description "ok";"#;
        let (stmts, errors) = parse_yang(src, file());
        assert!(errors.is_empty(), "errors: {:?}", errors);
        assert_eq!(stmts[0].arg_str(), "ok");
    }

    #[test]
    fn parse_extension_keyword() {
        let src = r#"mymod:my-ext "arg";"#;
        let (stmts, errors) = parse_yang(src, file());
        assert!(errors.is_empty(), "errors: {:?}", errors);
        assert!(
            matches!(&stmts[0].keyword, Keyword::ExtensionPrefixed { prefix, name }
                if prefix == "mymod" && name == "my-ext")
        );
    }

    #[test]
    fn parse_unquoted_arg_with_bracket_predicate() {
        // RFC 7950 §6.1.3: single-quote is not a stop character for unquoted
        // strings.  tailf:annotate-statement uses this syntax to target
        // specific grouping/list instances by name.
        let src = r#"tailf:annotate-statement "grouping[name='foo']" { tailf:info "x"; }"#;
        let (stmts, errors) = parse_yang(src, file());
        assert!(errors.is_empty(), "errors: {:?}", errors);
        assert_eq!(stmts[0].arg_str(), "grouping[name='foo']");
    }

    #[test]
    fn parse_unquoted_arg_containing_single_quote() {
        // An unquoted argument that embeds a single-quote mid-token must be
        // consumed as one token, not split at the quote character.
        let src = "description it's-a-test;";
        let (stmts, errors) = parse_yang(src, file());
        assert!(errors.is_empty(), "errors: {:?}", errors);
        assert_eq!(stmts[0].arg_str(), "it's-a-test");
    }
}
