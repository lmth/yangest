// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! YIN output plugin (`--format yin`).
//!
//! Converts the raw parsed YANG AST into its YIN XML representation per
//! RFC 7950 §13.

use std::io::Write;
use std::sync::Arc;

use yangest_core::ast::{BuiltInKeyword, Keyword, Stmt};
use yangest_core::compiler::{CompiledModule, ExpansionCtx, ModuleRegistry};

use yangest_core::plugin::{BundleState, EmitState, Plugin};

pub struct YinPlugin;

impl Plugin for YinPlugin {
    fn name(&self) -> &'static str {
        "yin"
    }

    fn emit(
        &self,
        modules: &[Arc<CompiledModule>],
        registry: &ModuleRegistry,
        ctx: &ExpansionCtx<'_>,
        out: &mut dyn Write,
    ) -> std::io::Result<()> {
        let bundle = self.prepare_bundle(modules, registry, ctx);
        writeln!(out, r#"<?xml version="1.0" encoding="UTF-8"?>"#)?;
        for module in modules {
            self.emit_module(module, registry, ctx, &bundle, &mut EmitState::new(), out)?;
        }
        Ok(())
    }

    fn emit_module(
        &self,
        module: &Arc<CompiledModule>,
        registry: &ModuleRegistry,
        _ctx: &ExpansionCtx<'_>,
        _bundle: &BundleState,
        _state: &mut EmitState,
        out: &mut dyn Write,
    ) -> std::io::Result<()> {
        let state = State::new(module, registry);
        print_stmt(&module.stmt, 0, &state, out)
    }
}

// ── Argument attribute mapping ────────────────────────────────────────────────

/// For each built-in YANG keyword: `(attribute_name, yin_element)`.
///
/// `yin_element = true` means the argument is emitted as a `<text>` child
/// element rather than an XML attribute.
fn yin_arg(kw: BuiltInKeyword) -> Option<(&'static str, bool)> {
    Some(match kw {
        BuiltInKeyword::Action => ("name", false),
        BuiltInKeyword::AnyData => ("name", false),
        BuiltInKeyword::AnyXml => ("name", false),
        BuiltInKeyword::Argument => ("name", false),
        BuiltInKeyword::Augment => ("target-node", false),
        BuiltInKeyword::Base => ("name", false),
        BuiltInKeyword::BelongsTo => ("module", false),
        BuiltInKeyword::Bit => ("name", false),
        BuiltInKeyword::Case => ("name", false),
        BuiltInKeyword::Choice => ("name", false),
        BuiltInKeyword::Config => ("value", false),
        BuiltInKeyword::Contact => ("text", true),
        BuiltInKeyword::Container => ("name", false),
        BuiltInKeyword::Default => ("value", false),
        BuiltInKeyword::Description => ("text", true),
        BuiltInKeyword::Deviate => ("value", false),
        BuiltInKeyword::Deviation => ("target-node", false),
        BuiltInKeyword::EnumStmt => ("name", false),
        BuiltInKeyword::ErrorAppTag => ("value", false),
        BuiltInKeyword::ErrorMessage => ("value", true),
        BuiltInKeyword::Extension => ("name", false),
        BuiltInKeyword::Feature => ("name", false),
        BuiltInKeyword::FractionDigits => ("value", false),
        BuiltInKeyword::Grouping => ("name", false),
        BuiltInKeyword::Identity => ("name", false),
        BuiltInKeyword::IfFeature => ("name", false),
        BuiltInKeyword::Import => ("module", false),
        BuiltInKeyword::Include => ("module", false),
        BuiltInKeyword::Input => return None, // no arg
        BuiltInKeyword::Key => ("value", false),
        BuiltInKeyword::Leaf => ("name", false),
        BuiltInKeyword::LeafList => ("name", false),
        BuiltInKeyword::Length => ("value", false),
        BuiltInKeyword::List => ("name", false),
        BuiltInKeyword::Mandatory => ("value", false),
        BuiltInKeyword::MaxElements => ("value", false),
        BuiltInKeyword::MinElements => ("value", false),
        BuiltInKeyword::Modifier => ("value", false),
        BuiltInKeyword::Module => ("name", false),
        BuiltInKeyword::Must => ("condition", false),
        BuiltInKeyword::Namespace => ("uri", false),
        BuiltInKeyword::Notification => ("name", false),
        BuiltInKeyword::OrderedBy => ("value", false),
        BuiltInKeyword::Organization => ("text", true),
        BuiltInKeyword::Output => return None, // no arg
        BuiltInKeyword::Path => ("value", false),
        BuiltInKeyword::Pattern => ("value", false),
        BuiltInKeyword::Position => ("value", false),
        BuiltInKeyword::Presence => ("value", false),
        BuiltInKeyword::Prefix => ("value", false),
        BuiltInKeyword::Range => ("value", false),
        BuiltInKeyword::Reference => ("text", true),
        BuiltInKeyword::Refine => ("target-node", false),
        BuiltInKeyword::RequireInstance => ("value", false),
        BuiltInKeyword::Revision => ("date", false),
        BuiltInKeyword::RevisionDate => ("date", false),
        BuiltInKeyword::Rpc => ("name", false),
        BuiltInKeyword::Status => ("value", false),
        BuiltInKeyword::Submodule => ("name", false),
        BuiltInKeyword::Type => ("name", false),
        BuiltInKeyword::Typedef => ("name", false),
        BuiltInKeyword::Unique => ("tag", false),
        BuiltInKeyword::Units => ("name", false),
        BuiltInKeyword::Uses => ("name", false),
        BuiltInKeyword::Value => ("value", false),
        BuiltInKeyword::When => ("condition", false),
        BuiltInKeyword::YangVersion => ("value", false),
        BuiltInKeyword::YinElement => ("value", false),
    })
}

// ── State threaded through recursive calls ────────────────────────────────────

struct State<'a> {
    /// prefix → namespace_uri for all imported modules + own module.
    xmlns: std::collections::HashMap<String, String>,
    /// Own module's prefix.
    own_prefix: String,
    /// Imports in document order: (prefix, namespace).
    imports_ordered: Vec<(String, String)>,
    /// Registry (for extension argument lookup).
    registry: &'a ModuleRegistry,
    /// prefix → module_name for the compiled module.
    prefix_to_mod: std::collections::HashMap<String, String>,
}

impl<'a> State<'a> {
    fn new(module: &CompiledModule, registry: &'a ModuleRegistry) -> Self {
        let mut xmlns = std::collections::HashMap::new();
        let mut prefix_to_mod = std::collections::HashMap::new();

        // Own namespace.
        xmlns.insert(module.prefix.clone(), module.namespace.clone());
        prefix_to_mod.insert(module.prefix.clone(), module.key.name.clone());

        // Import namespaces from prefix_map.
        for (prefix, mod_name) in &module.prefix_map {
            prefix_to_mod.insert(prefix.clone(), mod_name.clone());
            if let Some(imported) = registry.resolve_import(mod_name, None) {
                xmlns.insert(prefix.clone(), imported.namespace.clone());
            }
        }

        // Collect imports in document order from the raw stmt tree.
        let imports_ordered = collect_imports_ordered(&module.stmt, &xmlns);

        State {
            xmlns,
            own_prefix: module.prefix.clone(),
            imports_ordered,
            registry,
            prefix_to_mod,
        }
    }
}

/// Walk module stmt and collect (prefix, namespace) for each `import` substmt
/// in document order.
fn collect_imports_ordered(
    module_stmt: &Stmt,
    xmlns: &std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut result = Vec::new();
    for sub in &module_stmt.substmts {
        if !matches!(&sub.keyword, Keyword::BuiltIn(BuiltInKeyword::Import)) {
            continue;
        }
        let prefix = sub.substmts.iter().find_map(|s| {
            if matches!(&s.keyword, Keyword::BuiltIn(BuiltInKeyword::Prefix)) {
                s.arg.clone()
            } else {
                None
            }
        });
        if let Some(pfx) = prefix {
            if let Some(ns) = xmlns.get(&pfx) {
                result.push((pfx, ns.clone()));
            }
        }
    }
    // Emit the xmlns attributes in reverse import-declaration order.
    result.reverse();
    result
}

// ── XML helpers ───────────────────────────────────────────────────────────────

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn indent_str(lvl: usize) -> String {
    "  ".repeat(lvl)
}

fn element_name(kw: &Keyword) -> String {
    match kw {
        Keyword::BuiltIn(b) => b.as_str().to_owned(),
        Keyword::ExtensionPrefixed { prefix, name } => format!("{}:{}", prefix, name),
        Keyword::Extension { module, name } => format!("{}:{}", module, name),
    }
}

// ── Statement printer ─────────────────────────────────────────────────────────

fn print_stmt(stmt: &Stmt, lvl: usize, state: &State, out: &mut dyn Write) -> std::io::Result<()> {
    let ind = indent_str(lvl);
    let tag = element_name(&stmt.keyword);

    // Extension statements — handled separately.
    if let Keyword::ExtensionPrefixed { prefix, name }
    | Keyword::Extension {
        module: prefix,
        name,
    } = &stmt.keyword
    {
        let mod_name = state.prefix_to_mod.get(prefix.as_str());
        let ext_arg = mod_name
            .and_then(|mn| state.registry.resolve_import(mn, None))
            .and_then(|ext_mod| find_extension_arg(&ext_mod.stmt, name));
        return print_ext_stmt(stmt, lvl, state, out, &tag, ext_arg);
    }

    let b = match &stmt.keyword {
        Keyword::BuiltIn(b) => *b,
        _ => unreachable!(),
    };

    // module / submodule: special xmlns declaration handling.
    if matches!(b, BuiltInKeyword::Module | BuiltInKeyword::Submodule) {
        return print_module_stmt(stmt, lvl, state, out, &tag, b);
    }

    let arg_opt = yin_arg(b);
    let has_subs = !stmt.substmts.is_empty();

    match arg_opt {
        None => {
            // No argument (input, output).
            emit_block_or_empty(&ind, &tag, &stmt.substmts, lvl, state, out)?;
        }
        Some((attr, false)) => {
            let arg_val = escape_xml(stmt.arg.as_deref().unwrap_or(""));
            if has_subs {
                writeln!(out, "{}<{} {}=\"{}\">", ind, tag, attr, arg_val)?;
                for sub in &stmt.substmts {
                    print_stmt(sub, lvl + 1, state, out)?;
                }
                writeln!(out, "{}</{}>", ind, tag)?;
            } else {
                writeln!(out, "{}<{} {}=\"{}\"/>", ind, tag, attr, arg_val)?;
            }
        }
        Some((attr, true)) => {
            // Argument is a <{attr}> text child element.
            let raw = stmt.arg.as_deref().unwrap_or("");
            writeln!(out, "{}<{}>", ind, tag)?;
            print_text_element(out, raw, attr, lvl + 1)?;
            for sub in &stmt.substmts {
                print_stmt(sub, lvl + 1, state, out)?;
            }
            writeln!(out, "{}</{}>", ind, tag)?;
        }
    }

    Ok(())
}

fn print_module_stmt(
    stmt: &Stmt,
    lvl: usize,
    state: &State,
    out: &mut dyn Write,
    tag: &str,
    _b: BuiltInKeyword,
) -> std::io::Result<()> {
    let ind = indent_str(lvl);
    // xmlns attributes are indented at (lvl+4)*2 spaces.
    let xmlns_ind = indent_str(lvl + 4);
    let mod_name = stmt.arg.as_deref().unwrap_or("");

    write!(out, "{}<{} name=\"{}\"", ind, tag, escape_xml(mod_name))?;
    write!(
        out,
        "\n{}xmlns=\"urn:ietf:params:xml:ns:yang:yin:1\"",
        xmlns_ind
    )?;

    // Own namespace.
    if let Some(ns) = state.xmlns.get(&state.own_prefix) {
        write!(
            out,
            "\n{}xmlns:{}=\"{}\"",
            xmlns_ind,
            state.own_prefix,
            escape_xml(ns)
        )?;
    }

    // Imported namespaces in document order.
    for (pfx, ns) in &state.imports_ordered {
        write!(out, "\n{}xmlns:{}=\"{}\"", xmlns_ind, pfx, escape_xml(ns))?;
    }

    writeln!(out, ">")?;

    for sub in &stmt.substmts {
        print_stmt(sub, lvl + 1, state, out)?;
    }
    writeln!(out, "{}</{}>", ind, tag)?;
    Ok(())
}

fn print_ext_stmt(
    stmt: &Stmt,
    lvl: usize,
    state: &State,
    out: &mut dyn Write,
    tag: &str,
    ext_arg: Option<(String, bool)>,
) -> std::io::Result<()> {
    let ind = indent_str(lvl);
    let has_subs = !stmt.substmts.is_empty();

    match ext_arg {
        None => {
            emit_block_or_empty(&ind, tag, &stmt.substmts, lvl, state, out)?;
        }
        Some((attr, false)) => {
            let arg_val = escape_xml(stmt.arg.as_deref().unwrap_or(""));
            if has_subs {
                writeln!(out, "{}<{} {}=\"{}\">", ind, tag, attr, arg_val)?;
                for sub in &stmt.substmts {
                    print_stmt(sub, lvl + 1, state, out)?;
                }
                writeln!(out, "{}</{}>", ind, tag)?;
            } else {
                writeln!(out, "{}<{} {}=\"{}\"/>", ind, tag, attr, arg_val)?;
            }
        }
        Some((attr, true)) => {
            let raw = stmt.arg.as_deref().unwrap_or("");
            writeln!(out, "{}<{}>", ind, tag)?;
            print_text_element(out, raw, &attr, lvl + 1)?;
            for sub in &stmt.substmts {
                print_stmt(sub, lvl + 1, state, out)?;
            }
            writeln!(out, "{}</{}>", ind, tag)?;
        }
    }
    Ok(())
}

fn emit_block_or_empty(
    ind: &str,
    tag: &str,
    substmts: &[Stmt],
    lvl: usize,
    state: &State,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    if substmts.is_empty() {
        writeln!(out, "{}<{}/>", ind, tag)
    } else {
        writeln!(out, "{}<{}>", ind, tag)?;
        for sub in substmts {
            print_stmt(sub, lvl + 1, state, out)?;
        }
        writeln!(out, "{}</{}>", ind, tag)
    }
}

/// Emit `<{attr}>{content}</{attr}>` in a non-pretty format:
/// - First line of content immediately after `<{attr}>`
/// - Continuation lines start at column 0 (no indent)
/// - Closing `</{attr}>` immediately after the last content character
fn print_text_element(
    out: &mut dyn Write,
    text: &str,
    attr: &str,
    lvl: usize,
) -> std::io::Result<()> {
    let ind = indent_str(lvl);
    let escaped = escape_xml(text);
    let mut lines = escaped.split('\n');
    let first = lines.next().unwrap_or("");

    write!(out, "{}<{}>", ind, attr)?;
    write!(out, "{}", first)?;
    for line in lines {
        write!(out, "\n{}", line)?;
    }
    writeln!(out, "</{}>", attr)?;
    Ok(())
}

// ── Extension argument lookup ─────────────────────────────────────────────────

fn find_extension_arg(module_stmt: &Stmt, ext_name: &str) -> Option<(String, bool)> {
    for sub in &module_stmt.substmts {
        if matches!(&sub.keyword, Keyword::BuiltIn(BuiltInKeyword::Extension))
            && sub.arg.as_deref() == Some(ext_name)
        {
            for arg_stmt in &sub.substmts {
                if matches!(
                    &arg_stmt.keyword,
                    Keyword::BuiltIn(BuiltInKeyword::Argument)
                ) {
                    let arg_name = arg_stmt.arg.clone().unwrap_or_default();
                    let yin_el = arg_stmt.substmts.iter().any(|s| {
                        matches!(&s.keyword, Keyword::BuiltIn(BuiltInKeyword::YinElement))
                            && s.arg.as_deref() == Some("true")
                    });
                    return Some((arg_name, yin_el));
                }
            }
            return None;
        }
    }
    None
}

inventory::submit! {
    yangest_core::plugin::PluginRegistration { factory: || Box::new(YinPlugin) }
}
