// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! `--format yang-expanded` — prints the compiled YANG schema tree as YANG text.
//!
//! Unlike `--format yang`, which re-serialises the raw parsed AST, this format
//! walks the compiled tree via `module.children(ctx)`, which means:
//!  - `uses`/`grouping` statements are replaced by their expanded bodies.
//!  - Deviations are applied; deviated nodes are updated or removed.
//!  - Nodes disabled by `--feature` guards are omitted.
//!  - `--max-status` pruning is applied.

use std::io::Write;
use std::sync::Arc;

use yangest_core::ast::{BuiltInKeyword, Keyword};
use yangest_core::compiler::{
    CompiledModule, ExpansionCtx, ExtensionInstance, IfFeatureExpr, ModuleRegistry, MustExpr,
    OrderedBy, SchemaNode, SchemaNodeKind, Status, WhenExpr,
};
use yangest_core::plugin::{BundleState, EmitState, Plugin};

use crate::{escape_dq, format_arg, kwd_class, kwd_with_trailing_nl, print_stmt,
            write_hanging_string, KwdClass};

pub struct YangExpandedPlugin;

impl Plugin for YangExpandedPlugin {
    fn name(&self) -> &'static str {
        "yang-expanded"
    }

    fn extension(&self) -> &'static str {
        "yang"
    }

    fn emit_module(
        &self,
        module: &Arc<CompiledModule>,
        _registry: &ModuleRegistry,
        ctx: &ExpansionCtx<'_>,
        _bundle: &BundleState,
        _state: &mut EmitState,
        out: &mut dyn Write,
    ) -> std::io::Result<()> {
        writeln!(out, "module {} {{", module.key.name)?;

        // Print header / meta / defs from raw AST, skipping data nodes and grouping/augment/deviation
        let mut prev: Option<KwdClass> = Some(KwdClass::Module);
        for sub in &module.stmt.substmts {
            if is_body_stmt(&sub.keyword) {
                continue;
            }
            let cls = kwd_class(&sub.keyword);
            // Blank line between section classes at module body level
            if let Some(p) = prev {
                let class_break =
                    cls != p && cls != KwdClass::ExtStmt && p != KwdClass::Module;
                if class_break || kwd_with_trailing_nl(&sub.keyword) {
                    writeln!(out)?;
                }
            }
            // Print with None for prev_class; we manage the blank lines above
            print_stmt(sub, 1, None, out)?;
            prev = Some(cls);
        }

        // Print the data tree from the compiled tree
        let children = module.children(ctx);
        if !children.is_empty() {
            writeln!(out)?;
            for node in &children {
                emit_schema_node(node, module, ctx, 1, out)?;
            }
        }

        writeln!(out, "}}")
    }
}

/// Returns true for data-node and expansion-related keywords that must be
/// skipped from the raw AST (they are replaced by the compiled tree).
fn is_body_stmt(kw: &Keyword) -> bool {
    matches!(
        kw,
        Keyword::BuiltIn(
            BuiltInKeyword::Grouping
                | BuiltInKeyword::Uses
                | BuiltInKeyword::Augment
                | BuiltInKeyword::Deviation
                | BuiltInKeyword::Container
                | BuiltInKeyword::Leaf
                | BuiltInKeyword::LeafList
                | BuiltInKeyword::List
                | BuiltInKeyword::Choice
                | BuiltInKeyword::Case
                | BuiltInKeyword::AnyXml
                | BuiltInKeyword::AnyData
                | BuiltInKeyword::Rpc
                | BuiltInKeyword::Action
                | BuiltInKeyword::Input
                | BuiltInKeyword::Output
                | BuiltInKeyword::Notification
        )
    )
}

// ── Schema-node emitter ───────────────────────────────────────────────────────

fn emit_schema_node(
    node: &SchemaNode,
    module: &CompiledModule,
    ctx: &ExpansionCtx<'_>,
    depth: usize,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let ind = "  ".repeat(depth);
    match &node.kind {
        SchemaNodeKind::Container { presence, musts, .. } => {
            writeln!(out, "{}container {} {{", ind, node.name)?;
            emit_common_stmts(node, module, depth + 1, out)?;
            if let Some(p) = presence {
                let ind2 = "  ".repeat(depth + 1);
                writeln!(out, "{}presence \"{}\";", ind2, escape_dq(p))?;
            }
            for m in musts {
                emit_must(m, depth + 1, out)?;
            }
            for child in node.children(ctx) {
                emit_schema_node(&child, module, ctx, depth + 1, out)?;
            }
            writeln!(out, "{}}}", ind)?;
        }

        SchemaNodeKind::Leaf { type_stmt, units, default, mandatory, musts } => {
            writeln!(out, "{}leaf {} {{", ind, node.name)?;
            emit_common_stmts(node, module, depth + 1, out)?;
            print_stmt(type_stmt, depth + 1, None, out)?;
            let ind2 = "  ".repeat(depth + 1);
            if let Some(u) = units {
                writeln!(out, "{}units \"{}\";", ind2, escape_dq(u))?;
            }
            if let Some(d) = default {
                writeln!(out, "{}default \"{}\";", ind2, escape_dq(d))?;
            }
            if *mandatory {
                writeln!(out, "{}mandatory true;", ind2)?;
            }
            for m in musts {
                emit_must(m, depth + 1, out)?;
            }
            writeln!(out, "{}}}", ind)?;
        }

        SchemaNodeKind::LeafList {
            type_stmt,
            units,
            default,
            min_elements,
            max_elements,
            ordered_by,
            musts,
        } => {
            writeln!(out, "{}leaf-list {} {{", ind, node.name)?;
            emit_common_stmts(node, module, depth + 1, out)?;
            print_stmt(type_stmt, depth + 1, None, out)?;
            let ind2 = "  ".repeat(depth + 1);
            if let Some(u) = units {
                writeln!(out, "{}units \"{}\";", ind2, escape_dq(u))?;
            }
            for d in default {
                writeln!(out, "{}default \"{}\";", ind2, escape_dq(d))?;
            }
            if *min_elements > 0 {
                writeln!(out, "{}min-elements {};", ind2, min_elements)?;
            }
            if let Some(max) = max_elements {
                writeln!(out, "{}max-elements {};", ind2, max)?;
            }
            if *ordered_by == OrderedBy::User {
                writeln!(out, "{}ordered-by user;", ind2)?;
            }
            for m in musts {
                emit_must(m, depth + 1, out)?;
            }
            writeln!(out, "{}}}", ind)?;
        }

        SchemaNodeKind::List {
            key,
            unique,
            min_elements,
            max_elements,
            ordered_by,
            musts,
            ..
        } => {
            writeln!(out, "{}list {} {{", ind, node.name)?;
            emit_common_stmts(node, module, depth + 1, out)?;
            let ind2 = "  ".repeat(depth + 1);
            if !key.is_empty() {
                writeln!(out, "{}key \"{}\";", ind2, key.join(" "))?;
            }
            for u in unique {
                writeln!(out, "{}unique \"{}\";", ind2, escape_dq(u))?;
            }
            if *min_elements > 0 {
                writeln!(out, "{}min-elements {};", ind2, min_elements)?;
            }
            if let Some(max) = max_elements {
                writeln!(out, "{}max-elements {};", ind2, max)?;
            }
            if *ordered_by == OrderedBy::User {
                writeln!(out, "{}ordered-by user;", ind2)?;
            }
            for m in musts {
                emit_must(m, depth + 1, out)?;
            }
            for child in node.children(ctx) {
                emit_schema_node(&child, module, ctx, depth + 1, out)?;
            }
            writeln!(out, "{}}}", ind)?;
        }

        SchemaNodeKind::Choice { default, mandatory, .. } => {
            writeln!(out, "{}choice {} {{", ind, node.name)?;
            emit_common_stmts(node, module, depth + 1, out)?;
            let ind2 = "  ".repeat(depth + 1);
            if let Some(d) = default {
                writeln!(out, "{}default \"{}\";", ind2, escape_dq(d))?;
            }
            if *mandatory {
                writeln!(out, "{}mandatory true;", ind2)?;
            }
            for case in node.children(ctx) {
                emit_schema_node(&case, module, ctx, depth + 1, out)?;
            }
            writeln!(out, "{}}}", ind)?;
        }

        SchemaNodeKind::Case { .. } => {
            writeln!(out, "{}case {} {{", ind, node.name)?;
            emit_common_stmts(node, module, depth + 1, out)?;
            for child in node.children(ctx) {
                emit_schema_node(&child, module, ctx, depth + 1, out)?;
            }
            writeln!(out, "{}}}", ind)?;
        }

        SchemaNodeKind::Rpc { musts, .. } => {
            writeln!(out, "{}rpc {} {{", ind, node.name)?;
            emit_common_stmts(node, module, depth + 1, out)?;
            for m in musts {
                emit_must(m, depth + 1, out)?;
            }
            let input = node.input_children(ctx);
            if !input.is_empty() {
                let ind2 = "  ".repeat(depth + 1);
                writeln!(out, "{}input {{", ind2)?;
                for child in &input {
                    emit_schema_node(child, module, ctx, depth + 2, out)?;
                }
                writeln!(out, "{}}}", ind2)?;
            }
            let output = node.output_children(ctx);
            if !output.is_empty() {
                let ind2 = "  ".repeat(depth + 1);
                writeln!(out, "{}output {{", ind2)?;
                for child in &output {
                    emit_schema_node(child, module, ctx, depth + 2, out)?;
                }
                writeln!(out, "{}}}", ind2)?;
            }
            writeln!(out, "{}}}", ind)?;
        }

        SchemaNodeKind::Action { .. } => {
            writeln!(out, "{}action {} {{", ind, node.name)?;
            emit_common_stmts(node, module, depth + 1, out)?;
            let input = node.input_children(ctx);
            if !input.is_empty() {
                let ind2 = "  ".repeat(depth + 1);
                writeln!(out, "{}input {{", ind2)?;
                for child in &input {
                    emit_schema_node(child, module, ctx, depth + 2, out)?;
                }
                writeln!(out, "{}}}", ind2)?;
            }
            let output = node.output_children(ctx);
            if !output.is_empty() {
                let ind2 = "  ".repeat(depth + 1);
                writeln!(out, "{}output {{", ind2)?;
                for child in &output {
                    emit_schema_node(child, module, ctx, depth + 2, out)?;
                }
                writeln!(out, "{}}}", ind2)?;
            }
            writeln!(out, "{}}}", ind)?;
        }

        SchemaNodeKind::Notification { musts, .. } => {
            writeln!(out, "{}notification {} {{", ind, node.name)?;
            emit_common_stmts(node, module, depth + 1, out)?;
            for m in musts {
                emit_must(m, depth + 1, out)?;
            }
            for child in node.children(ctx) {
                emit_schema_node(&child, module, ctx, depth + 1, out)?;
            }
            writeln!(out, "{}}}", ind)?;
        }

        SchemaNodeKind::AnyXml { mandatory, musts } | SchemaNodeKind::AnyData { mandatory, musts } => {
            let kw = match &node.kind {
                SchemaNodeKind::AnyXml { .. } => "anyxml",
                _ => "anydata",
            };
            writeln!(out, "{}{} {} {{", ind, kw, node.name)?;
            emit_common_stmts(node, module, depth + 1, out)?;
            let ind2 = "  ".repeat(depth + 1);
            if *mandatory {
                writeln!(out, "{}mandatory true;", ind2)?;
            }
            for m in musts {
                emit_must(m, depth + 1, out)?;
            }
            writeln!(out, "{}}}", ind)?;
        }

        SchemaNodeKind::Uses { .. } => {
            // Uses nodes are expanded by children(ctx); this variant should not appear here.
        }
    }
    Ok(())
}

// ── Common sub-statements ─────────────────────────────────────────────────────

/// Emit sub-statements common to all schema nodes: if-feature, when, status,
/// config, description, reference, extension instances.
fn emit_common_stmts(
    node: &SchemaNode,
    module: &CompiledModule,
    depth: usize,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let ind = "  ".repeat(depth);
    let if_kw = Keyword::BuiltIn(BuiltInKeyword::IfFeature);
    for iff in &node.if_features {
        let expr_str = if_feature_to_str(iff, module);
        writeln!(out, "{}if-feature {};", ind, format_arg(&expr_str, &if_kw))?;
    }
    for w in &node.when {
        emit_when(w, depth, out)?;
    }
    match node.status {
        Status::Current => {}
        Status::Deprecated => writeln!(out, "{}status deprecated;", ind)?,
        Status::Obsolete => writeln!(out, "{}status obsolete;", ind)?,
    }
    if let Some(cfg) = node.config {
        writeln!(out, "{}config {};", ind, if cfg { "true" } else { "false" })?;
    }
    if let Some(desc) = &node.description {
        writeln!(out, "{}description", ind)?;
        write_hanging_string(out, desc, depth)?;
        writeln!(out, ";")?;
    }
    if let Some(reference) = &node.reference {
        if reference.contains('\n') {
            writeln!(out, "{}reference", ind)?;
            write_hanging_string(out, reference, depth)?;
            writeln!(out, ";")?;
        } else {
            writeln!(out, "{}reference \"{}\";", ind, escape_dq(reference))?;
        }
    }
    for ext in &node.extensions {
        emit_extension_instance(ext, module, depth, out)?;
    }
    Ok(())
}

// ── when / must / extension helpers ──────────────────────────────────────────

fn emit_when(w: &WhenExpr, depth: usize, out: &mut dyn Write) -> std::io::Result<()> {
    let ind = "  ".repeat(depth);
    let when_kw = Keyword::BuiltIn(BuiltInKeyword::When);
    let arg = format_arg(&w.xpath, &when_kw);
    if w.description.is_none() && w.reference.is_none() {
        writeln!(out, "{}when {};", ind, arg)
    } else {
        writeln!(out, "{}when {} {{", ind, arg)?;
        let ind2 = "  ".repeat(depth + 1);
        if let Some(desc) = &w.description {
            writeln!(out, "{}description", ind2)?;
            write_hanging_string(out, desc, depth + 1)?;
            writeln!(out, ";")?;
        }
        if let Some(reference) = &w.reference {
            writeln!(out, "{}reference \"{}\";", ind2, escape_dq(reference))?;
        }
        writeln!(out, "{}}}", ind)
    }
}

fn emit_must(m: &MustExpr, depth: usize, out: &mut dyn Write) -> std::io::Result<()> {
    let ind = "  ".repeat(depth);
    let must_kw = Keyword::BuiltIn(BuiltInKeyword::Must);
    let arg = format_arg(&m.xpath, &must_kw);
    let has_subs =
        m.error_message.is_some() || m.error_app_tag.is_some() || m.description.is_some();
    if !has_subs {
        writeln!(out, "{}must {};", ind, arg)
    } else {
        writeln!(out, "{}must {} {{", ind, arg)?;
        let ind2 = "  ".repeat(depth + 1);
        if let Some(em) = &m.error_message {
            writeln!(out, "{}error-message \"{}\";", ind2, escape_dq(em))?;
        }
        if let Some(eat) = &m.error_app_tag {
            writeln!(out, "{}error-app-tag \"{}\";", ind2, escape_dq(eat))?;
        }
        if let Some(desc) = &m.description {
            writeln!(out, "{}description", ind2)?;
            write_hanging_string(out, desc, depth + 1)?;
            writeln!(out, ";")?;
        }
        writeln!(out, "{}}}", ind)
    }
}

fn emit_extension_instance(
    ext: &ExtensionInstance,
    module: &CompiledModule,
    depth: usize,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let ind = "  ".repeat(depth);
    // Resolve ext.module → prefix using this module's import prefix map
    let prefix = if ext.module == module.key.name {
        module.prefix.clone()
    } else {
        module
            .prefix_map
            .iter()
            .find(|(_, v)| v.as_str() == ext.module.as_str())
            .map(|(p, _)| p.clone())
            .unwrap_or_else(|| ext.module.clone())
    };
    let kw_str = format!("{}:{}", prefix, ext.name);
    let ext_kw = Keyword::ExtensionPrefixed {
        prefix: prefix.clone(),
        name: ext.name.clone(),
    };
    let has_subs = !ext.substmts.is_empty();
    if has_subs {
        match &ext.arg {
            None => writeln!(out, "{}{} {{", ind, kw_str)?,
            Some(arg) => writeln!(out, "{}{} {} {{", ind, kw_str, format_arg(arg, &ext_kw))?,
        }
        for sub in &ext.substmts {
            print_stmt(sub, depth + 1, None, out)?;
        }
        writeln!(out, "{}}}", ind)?;
    } else {
        match &ext.arg {
            None => writeln!(out, "{}{};", ind, kw_str)?,
            Some(arg) => writeln!(out, "{}{} {};", ind, kw_str, format_arg(arg, &ext_kw))?,
        }
    }
    Ok(())
}

// ── if-feature expression serialiser ─────────────────────────────────────────

/// Serialise a compiled `IfFeatureExpr` back to a YANG if-feature string.
/// Uses the prefix map of `module` to produce `prefix:feature` for cross-module
/// references.
fn if_feature_to_str(expr: &IfFeatureExpr, module: &CompiledModule) -> String {
    match expr {
        IfFeatureExpr::Name(feat, None) => feat.clone(),
        IfFeatureExpr::Name(feat, Some(mod_name)) => {
            if mod_name == &module.key.name {
                feat.clone()
            } else {
                let prefix = module
                    .prefix_map
                    .iter()
                    .find(|(_, v)| v.as_str() == mod_name.as_str())
                    .map(|(p, _)| p.as_str())
                    .unwrap_or(mod_name.as_str());
                format!("{}:{}", prefix, feat)
            }
        }
        IfFeatureExpr::Not(inner) => format!("not {}", if_feature_atom(inner, module)),
        IfFeatureExpr::And(a, b) => format!(
            "{} and {}",
            if_feature_atom(a, module),
            if_feature_atom(b, module)
        ),
        IfFeatureExpr::Or(a, b) => format!(
            "{} or {}",
            if_feature_to_str(a, module),
            if_feature_to_str(b, module)
        ),
    }
}

/// Wrap in parens only when needed for operator precedence.
/// `or` inside `and` or `not` needs parens; `and`/`name` do not.
fn if_feature_atom(expr: &IfFeatureExpr, module: &CompiledModule) -> String {
    match expr {
        IfFeatureExpr::Or(_, _) => format!("({})", if_feature_to_str(expr, module)),
        _ => if_feature_to_str(expr, module),
    }
}


