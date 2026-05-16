// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! YANG pretty-printer plugin (`--format yang`).
//!
//! Re-serializes the raw parsed AST back into YANG text, following the same
//! conventions as yanger's `yang` output format:
//! - 2-space indentation
//! - Blank lines between module-level sections (header → linkage → meta → defs, etc.)
//! - `description`, `organization`, `contact` arguments on a new "hanging" line
//! - Single quotes preferred for `pattern` / `when` / `must`
//! - Quoting rules matching yanger's `quote_pattern()`

mod expanded;
pub use expanded::YangExpandedPlugin;

use std::io::Write;
use std::sync::Arc;

use yangest_core::ast::{BuiltInKeyword, Keyword, Stmt};
use yangest_core::compiler::{CompiledModule, ExpansionCtx, ModuleRegistry};

use yangest_core::plugin::{EmitState, Plugin};

pub struct YangPlugin;

impl Plugin for YangPlugin {
    fn name(&self) -> &'static str {
        "yang"
    }

    fn emit_module(
        &self,
        module: &Arc<CompiledModule>,
        _registry: &ModuleRegistry,
        _ctx: &ExpansionCtx<'_>,
        _state: &mut EmitState,
        out: &mut dyn Write,
    ) -> std::io::Result<()> {
        print_stmt(&module.stmt, 0, None, out)
    }
}

// ── Keyword classification (mirrors yanger's kwd_class / kwd_with_trailing_nl) ─

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum KwdClass {
    Header,   // yang-version, namespace, prefix, belongs-to
    Linkage,  // import, include
    Meta,     // organization, contact, description, reference
    Revision, // revision
    Defs,     // typedef, grouping, identity, feature, extension
    Body,     // everything else (config nodes, deviation, …)
    ExtStmt,  // extension statements (prefix:local-name)
    Module,   // module, submodule (top-level container)
}

pub(crate) fn kwd_class(kw: &Keyword) -> KwdClass {
    match kw {
        Keyword::BuiltIn(b) => match b {
            BuiltInKeyword::YangVersion
            | BuiltInKeyword::Namespace
            | BuiltInKeyword::Prefix
            | BuiltInKeyword::BelongsTo => KwdClass::Header,
            BuiltInKeyword::Import | BuiltInKeyword::Include => KwdClass::Linkage,
            BuiltInKeyword::Organization
            | BuiltInKeyword::Contact
            | BuiltInKeyword::Description
            | BuiltInKeyword::Reference => KwdClass::Meta,
            BuiltInKeyword::Revision => KwdClass::Revision,
            BuiltInKeyword::Typedef
            | BuiltInKeyword::Grouping
            | BuiltInKeyword::Identity
            | BuiltInKeyword::Feature
            | BuiltInKeyword::Extension => KwdClass::Defs,
            BuiltInKeyword::Module | BuiltInKeyword::Submodule => KwdClass::Module,
            _ => KwdClass::Body,
        },
        Keyword::ExtensionPrefixed { .. } | Keyword::Extension { .. } => KwdClass::ExtStmt,
    }
}

/// Keywords after which an extra blank line is inserted even at deeper levels.
pub(crate) fn kwd_with_trailing_nl(kw: &Keyword) -> bool {
    matches!(
        kw,
        Keyword::BuiltIn(
            BuiltInKeyword::Typedef
                | BuiltInKeyword::Grouping
                | BuiltInKeyword::Identity
                | BuiltInKeyword::Feature
                | BuiltInKeyword::Extension
        )
    )
}

// ── Argument formatting ───────────────────────────────────────────────────────

/// Keywords whose argument is placed on the next line (not inline).
fn force_newline_arg(kw: &Keyword) -> bool {
    matches!(
        kw,
        Keyword::BuiltIn(
            BuiltInKeyword::Organization | BuiltInKeyword::Contact | BuiltInKeyword::Description
        )
    )
}

/// Argument style for a YANG keyword, mirroring yanger's `classify_quoting`.
enum ArgStyle {
    /// Never quote (e.g. `yang-version`).
    NeverQuote,
    /// Always double-quote (string-type arguments in the yang parser).
    ForceDoubleQuote,
    /// Prefer single-quote, fall back to double-quote.
    PreferSingleQuote,
    /// Quote only when `needs_quotes()` triggers, else unquoted.
    QuoteIfNeeded,
}

fn arg_style(kw: &Keyword) -> ArgStyle {
    match kw {
        // Extension statement arguments are always string-type → force dquote.
        Keyword::ExtensionPrefixed { .. } | Keyword::Extension { .. } => ArgStyle::ForceDoubleQuote,
        Keyword::BuiltIn(b) => match b {
            // Explicitly never quote.
            BuiltInKeyword::YangVersion => ArgStyle::NeverQuote,
            // Prefer single quote.
            BuiltInKeyword::Pattern | BuiltInKeyword::When | BuiltInKeyword::Must => {
                ArgStyle::PreferSingleQuote
            }
            // String-type arguments that must always be double-quoted, even if
            // their content looks like a plain identifier.
            BuiltInKeyword::Description
            | BuiltInKeyword::Organization
            | BuiltInKeyword::Contact
            | BuiltInKeyword::Reference
            | BuiltInKeyword::ErrorMessage
            | BuiltInKeyword::ErrorAppTag
            | BuiltInKeyword::Presence
            | BuiltInKeyword::Default
            | BuiltInKeyword::Key
            | BuiltInKeyword::Unique
            | BuiltInKeyword::Units
            | BuiltInKeyword::Length
            | BuiltInKeyword::Range
            | BuiltInKeyword::Namespace
            | BuiltInKeyword::Augment
            | BuiltInKeyword::Refine
            | BuiltInKeyword::Deviation
            | BuiltInKeyword::Path
            | BuiltInKeyword::EnumStmt => ArgStyle::ForceDoubleQuote,
            // Everything else: quote only if the content requires it.
            _ => ArgStyle::QuoteIfNeeded,
        },
    }
}

/// Keywords that prefer single-quoted strings when quoting is needed.
fn prefer_single_quote(kw: &Keyword) -> bool {
    matches!(arg_style(kw), ArgStyle::PreferSingleQuote)
}

/// Returns true when the argument string requires quoting.
/// Matches yanger's `quote_pattern()` — any of these substrings force a quote.
fn needs_quotes(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    s.contains(' ')
        || s.contains('\t')
        || s.contains('\n')
        || s.contains('{')
        || s.contains('}')
        || s.contains(';')
        || s.contains("//")
        || s.contains("/*")
        || s.contains("*/")
        || s.contains('\\')
        || s.contains('.')
        || s.contains('/')
        || s.contains('=')
        || s.contains("urn:")
        || s.contains('"')
        || s.contains('\'')
}

pub(crate) fn escape_dq(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\t', "\\t")
}

/// Format an argument for inline use on the same line as the keyword.
pub(crate) fn format_arg(arg: &str, kw: &Keyword) -> String {
    match arg_style(kw) {
        ArgStyle::NeverQuote => arg.to_owned(),
        ArgStyle::ForceDoubleQuote => format!("\"{}\"", escape_dq(arg)),
        ArgStyle::PreferSingleQuote => {
            if !arg.contains('\'') {
                format!("'{}'", arg)
            } else {
                format!("\"{}\"", escape_dq(arg))
            }
        }
        ArgStyle::QuoteIfNeeded => {
            if !needs_quotes(arg) {
                arg.to_owned()
            } else if prefer_single_quote(kw) && !arg.contains('\'') {
                format!("'{}'", arg)
            } else {
                format!("\"{}\"", escape_dq(arg))
            }
        }
    }
}

// ── Keyword string ────────────────────────────────────────────────────────────

fn kw_str(kw: &Keyword) -> String {
    match kw {
        Keyword::BuiltIn(b) => b.as_str().to_owned(),
        Keyword::ExtensionPrefixed { prefix, name } => format!("{}:{}", prefix, name),
        // Fully resolved extension: should not normally appear in the raw stmt,
        // but handle gracefully by emitting module:name.
        Keyword::Extension { module, name } => format!("{}:{}", module, name),
    }
}

// ── Statement printer ─────────────────────────────────────────────────────────

/// Recursively print a YANG statement.
///
/// `lvl` is the nesting depth (0 = top-level `module`).
/// `prev_class` is the `KwdClass` of the previous sibling, used to decide
/// whether a blank line should be inserted before this statement.
pub(crate) fn print_stmt(
    stmt: &Stmt,
    lvl: usize,
    prev_class: Option<KwdClass>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let indent = "  ".repeat(lvl);
    let kw = kw_str(&stmt.keyword);
    let cls = kwd_class(&stmt.keyword);

    // Insert blank line before this statement when:
    //   • at module-body level (lvl==1) and the keyword class changed, OR
    //   • this keyword type always gets a preceding blank line.
    if let Some(prev) = prev_class {
        let class_break =
            lvl == 1 && cls != prev && cls != KwdClass::ExtStmt && prev != KwdClass::Module;
        if class_break || kwd_with_trailing_nl(&stmt.keyword) {
            writeln!(out)?;
        }
    }

    let has_subs = !stmt.substmts.is_empty();

    if has_subs {
        // Block statement: `keyword [arg] { ... }`
        match &stmt.arg {
            None => writeln!(out, "{}{} {{", indent, kw)?,
            Some(arg) => writeln!(
                out,
                "{}{} {} {{",
                indent,
                kw,
                format_arg(arg, &stmt.keyword)
            )?,
        }
        let mut prev: Option<KwdClass> = None;
        for sub in &stmt.substmts {
            let sub_cls = kwd_class(&sub.keyword);
            print_stmt(sub, lvl + 1, prev, out)?;
            prev = Some(sub_cls);
        }
        writeln!(out, "{}}}", indent)?;
    } else {
        // Leaf statement: `keyword [arg];`
        match &stmt.arg {
            None => writeln!(out, "{}{};", indent, kw)?,
            Some(arg) => {
                let force_nl = force_newline_arg(&stmt.keyword);
                // Use hanging format if force_newline_arg or if the arg is a
                // dquote-style string that contains embedded newlines.
                let multiline_str = matches!(
                    arg_style(&stmt.keyword),
                    ArgStyle::ForceDoubleQuote | ArgStyle::PreferSingleQuote
                ) && arg.contains('\n');
                if force_nl || multiline_str {
                    writeln!(out, "{}{}", indent, kw)?;
                    write_hanging_string(out, arg, lvl + 1)?;
                    writeln!(out, ";")?;
                } else {
                    writeln!(out, "{}{} {};", indent, kw, format_arg(arg, &stmt.keyword))?;
                }
            }
        }
    }

    Ok(())
}

/// Write a (possibly multi-line) double-quoted string argument on its own line.
///
/// The opening `"` is placed at `depth * 2` spaces of indentation.
/// Continuation lines (after embedded `\n`) are indented one extra space so
/// their text aligns with the first character after the opening `"`.
pub(crate) fn write_hanging_string(out: &mut dyn Write, s: &str, depth: usize) -> std::io::Result<()> {
    let indent = "  ".repeat(depth);
    // One extra space after indent to align continuation with first char after `"`.
    let cont = format!("{} ", indent);

    let escaped = escape_dq(s);
    let mut lines = escaped.split('\n');

    let first = lines.next().unwrap_or("");
    write!(out, "{}\"{}", indent, first)?;
    for line in lines {
        if line.is_empty() {
            // Empty continuation lines must not carry trailing whitespace.
            write!(out, "\n")?;
        } else {
            write!(out, "\n{}{}", cont, line)?;
        }
    }
    write!(out, "\"")?;
    Ok(())
}

inventory::submit! {
    yangest_core::plugin::PluginRegistration { factory: || Box::new(YangPlugin) }
}

inventory::submit! {
    yangest_core::plugin::PluginRegistration { factory: || Box::new(YangExpandedPlugin) }
}
