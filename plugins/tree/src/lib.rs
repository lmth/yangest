// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! RFC 8340 tree output plugin.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use clap::{Arg, ArgMatches};
use yangest_core::compiler::{
    AugmentEntry, CompiledModule, ExpansionCtx, ModuleRegistry, PathStep, SchemaNode,
    SchemaNodeKind, Status, expand_children,
};
use yangest_core::plugin::{EmitState, Plugin};

// ── Public plugin struct ──────────────────────────────────────────────────────

/// Emits the RFC 8340 tree diagram for a YANG module.
#[derive(Default)]
pub struct TreePlugin {
    /// Maximum nesting depth to display (`None` = unlimited).
    depth: Option<usize>,
    /// Show only the subtree rooted at this path.
    /// Each element is a node name (e.g. `["native", "interface"]`).
    path_filter: Option<Vec<String>>,
}

impl Plugin for TreePlugin {
    fn name(&self) -> &'static str {
        "tree"
    }

    fn cli_args(&self) -> Vec<Arg> {
        vec![
            Arg::new("tree-depth")
                .long("tree-depth")
                .value_name("N")
                .help("Limit tree output to N levels of nesting (1 = direct children only)")
                .value_parser(clap::value_parser!(usize)),
            Arg::new("tree-path")
                .long("tree-path")
                .value_name("PATH")
                .help(
                    "Show only the subtree rooted at PATH. \
                     PATH is a slash-separated list of node names, \
                     e.g. /native/interface",
                ),
        ]
    }

    fn configure(&mut self, matches: &ArgMatches) {
        self.depth = matches.get_one::<usize>("tree-depth").copied();
        self.path_filter = matches.get_one::<String>("tree-path").map(|p| {
            p.split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        });
    }

    fn emit_module(
        &self,
        module: &Arc<CompiledModule>,
        registry: &ModuleRegistry,
        ctx: &ExpansionCtx<'_>,
        _state: &mut EmitState,
        out: &mut dyn Write,
    ) -> std::io::Result<()> {
        let all_top = module.children(ctx);

        // Resolve path filter to the target subtree.
        let filtered: Option<Vec<SchemaNode>>;
        let top_children: &[SchemaNode] = if let Some(path) = &self.path_filter {
            filtered = find_at_path(&all_top, path, ctx);
            match &filtered {
                Some(nodes) => nodes.as_slice(),
                None => return Ok(()), // path not found
            }
        } else {
            &all_top
        };

        if top_children.is_empty() && module.augments.is_empty() {
            return Ok(());
        }
        writeln!(out, "module: {}", module.key.name)?;

        if !top_children.is_empty() {
            let augments_in = if self.path_filter.is_none() {
                collect_incoming_augments(module, registry)
            } else {
                HashMap::new() // augments not shown when path-filtered
            };
            emit_children(
                top_children,
                &[],
                &augments_in,
                module,
                ctx,
                "  ",
                None,
                &[],
                0,
                self.depth,
                out,
            )?;
        }

        if self.path_filter.is_none() && !module.augments.is_empty() {
            writeln!(out)?;
            for aug in &module.augments {
                emit_augment_section(aug, module, ctx, self.depth, out)?;
            }
        }

        Ok(())
    }
}

// ── Path navigation ───────────────────────────────────────────────────────────

/// Walk `nodes` following `path` components, returning the node(s) at the
/// final path element (wrapped in a single-element vec), or `None` if any
/// component is not found.
fn find_at_path(
    nodes: &[SchemaNode],
    path: &[String],
    ctx: &ExpansionCtx<'_>,
) -> Option<Vec<SchemaNode>> {
    let (head, tail) = path.split_first()?;
    for node in nodes {
        if node.name == *head {
            if tail.is_empty() {
                return Some(vec![node.clone()]);
            }
            let children = node.children(ctx);
            return find_at_path(&children, tail, ctx);
        }
    }
    None
}

// ── Augment helpers ───────────────────────────────────────────────────────────

/// Maps an augment target path (as local names) to the groups of nodes to inject there.
type AugmentMap = HashMap<Vec<String>, Vec<(Arc<CompiledModule>, Vec<SchemaNode>)>>;

fn emit_augment_section(
    aug: &AugmentEntry,
    module: &CompiledModule,
    ctx: &ExpansionCtx<'_>,
    depth: Option<usize>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let path_str = format_augment_path(&aug.target_path);
    writeln!(out, "  augment {path_str}:")?;
    let empty_map = HashMap::new();
    let target_names: Vec<String> = aug
        .target_path
        .iter()
        .map(|step| step.name.clone())
        .collect();
    let nodes = expand_children(
        &aug.nodes,
        &module.prefix,
        &module.key.name,
        &module.overlay,
        &aug.target_path,
        ctx,
    );
    emit_children(
        &nodes,
        &target_names,
        &empty_map,
        module,
        ctx,
        "    ",
        None,
        &[],
        0,
        depth,
        out,
    )
}

/// Formats an augment target path in the canonical `/prefix:name/name` notation.
fn format_augment_path(path: &[PathStep]) -> String {
    use std::fmt::Write as FmtWrite;
    let mut s = String::new();
    for step in path {
        s.push('/');
        if let Some(pfx) = &step.prefix {
            let _ = write!(s, "{pfx}:");
        }
        s.push_str(&step.name);
    }
    s
}

fn collect_incoming_augments(target: &CompiledModule, registry: &ModuleRegistry) -> AugmentMap {
    let mut result: AugmentMap = HashMap::new();
    for other in registry.modules.values() {
        let mut target_augments: Vec<(Arc<CompiledModule>, Vec<String>, Vec<SchemaNode>)> =
            Vec::new();
        for aug in &other.augments {
            if let Some(first) = aug.target_path.first() {
                let resolves_to = first
                    .prefix
                    .as_deref()
                    .and_then(|p| other.prefix_map.get(p))
                    .map(|s| s.as_str())
                    .unwrap_or(other.key.name.as_str());
                if resolves_to == target.key.name {
                    let path: Vec<String> = aug
                        .target_path
                        .iter()
                        .map(|step| step.name.clone())
                        .collect();
                    target_augments.push((Arc::clone(other), path, aug.nodes.clone()));
                }
            }
        }
        if target_augments.is_empty() {
            continue;
        }

        let mut first_claim: HashMap<Vec<String>, HashMap<String, usize>> = HashMap::new();
        for (i, (_, path, nodes)) in target_augments.iter().enumerate() {
            for node in nodes {
                first_claim
                    .entry(path.clone())
                    .or_default()
                    .entry(node.name.clone())
                    .or_insert(i);
            }
            for j in 1..path.len() {
                let parent = path[0..j].to_vec();
                let child_name = path[j].clone();
                first_claim
                    .entry(parent)
                    .or_default()
                    .entry(child_name)
                    .or_insert(i);
            }
        }

        let mut path_groups: HashMap<
            Vec<String>,
            Vec<(usize, usize, Arc<CompiledModule>, Vec<SchemaNode>)>,
        > = HashMap::new();
        for (i, (source_module, path, nodes)) in target_augments.into_iter().enumerate() {
            let eff_pos = nodes
                .iter()
                .filter_map(|n| first_claim.get(&path).and_then(|m| m.get(&n.name)).copied())
                .min()
                .unwrap_or(i);
            path_groups
                .entry(path)
                .or_default()
                .push((eff_pos, i, source_module, nodes));
        }

        for (path, mut groups) in path_groups {
            groups.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            for (_, _, source_module, nodes) in groups {
                result
                    .entry(path.clone())
                    .or_default()
                    .push((source_module, nodes));
            }
        }
    }
    result
}

// ── Display helpers ───────────────────────────────────────────────────────────

/// Returns the RFC 8340 display name for a schema node.
/// Nodes augmented from a foreign module are shown as `prefix:name`.
fn node_display_name(node: &SchemaNode, module: &CompiledModule) -> String {
    if node.module_name == module.key.name {
        node.name.clone()
    } else {
        let prefix = module
            .prefix_map
            .iter()
            .find(|(_, v)| v.as_str() == node.module_name.as_str())
            .map(|(pfx, _)| pfx.as_str())
            .unwrap_or(node.module_prefix.as_str());
        format!("{}:{}", prefix, node.name)
    }
}

fn compute_width(nodes: &[SchemaNode], module: &CompiledModule, ctx: &ExpansionCtx<'_>) -> usize {
    nodes
        .iter()
        .map(|n| match &n.kind {
            SchemaNodeKind::Choice { .. } => 3 + compute_width(&n.children(ctx), module, ctx),
            SchemaNodeKind::Case { .. } => 3 + compute_width(&n.children(ctx), module, ctx),
            _ => node_display_name(n, module).len(),
        })
        .max()
        .unwrap_or(0)
}

fn mk_name(name_with_suffix: &str, max_name_width: usize) -> String {
    let min_w = max_name_width + 1;
    let len = name_with_suffix.len();
    if len < min_w {
        format!("{}{}   ", name_with_suffix, " ".repeat(min_w - len))
    } else {
        format!("{}   ", name_with_suffix)
    }
}

// ── Core emit functions ───────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn emit_children(
    nodes: &[SchemaNode],
    parent_path: &[String],
    augments_in: &AugmentMap,
    module: &CompiledModule,
    ctx: &ExpansionCtx<'_>,
    prefix: &str,
    config_ctx: Option<bool>,
    list_keys: &[String],
    inherited_width: usize,
    depth_left: Option<usize>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    if depth_left == Some(0) {
        return Ok(());
    }
    let n = nodes.len();
    let width = if inherited_width > 0 {
        inherited_width
    } else {
        compute_width(nodes, module, ctx)
    };
    for (i, node) in nodes.iter().enumerate() {
        let is_last = i == n - 1;
        let child_prefix = if is_last {
            format!("{}   ", prefix)
        } else {
            format!("{}|  ", prefix)
        };
        emit_node(
            node,
            parent_path,
            augments_in,
            module,
            ctx,
            prefix,
            &child_prefix,
            config_ctx,
            list_keys,
            width,
            depth_left,
            out,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_node(
    node: &SchemaNode,
    parent_path: &[String],
    augments_in: &AugmentMap,
    module: &CompiledModule,
    ctx: &ExpansionCtx<'_>,
    prefix: &str,
    child_prefix: &str,
    config_ctx: Option<bool>,
    list_keys: &[String],
    width: usize,
    depth_left: Option<usize>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let eff_config = if config_ctx == Some(false) {
        Some(false)
    } else {
        node.config
    };

    let status = match node.status {
        Status::Deprecated => 'x',
        Status::Obsolete => 'o',
        Status::Current => '+',
    };

    let rw = rw_from_config(eff_config);
    let dname = node_display_name(node, module);

    let node_path: Vec<String> = {
        let mut p = parent_path.to_vec();
        p.push(node.name.clone());
        p
    };
    let schema_path = node.schema_path().unwrap_or_default();

    // Depth for children is one level less than current.
    let child_depth = depth_left.map(|d| d.saturating_sub(1));

    fn with_augments(
        children: &[SchemaNode],
        node_path: &[String],
        schema_path: &[PathStep],
        augments_in: &AugmentMap,
        ctx: &ExpansionCtx<'_>,
    ) -> Option<Vec<SchemaNode>> {
        augments_in.get(node_path).map(|groups| {
            let mut combined: Vec<SchemaNode> = children.to_vec();
            for (source_module, group) in groups {
                let mut expanded = expand_children(
                    group,
                    &source_module.prefix,
                    &source_module.key.name,
                    &source_module.overlay,
                    schema_path,
                    ctx,
                );
                combined.append(&mut expanded);
            }
            combined
        })
    }

    match &node.kind {
        SchemaNodeKind::Container { presence, .. } => {
            let p_marker = if presence.is_some() { "!" } else { "" };
            writeln!(out, "{}{}--{} {}{}", prefix, status, rw, dname, p_marker)?;
            let children = node.children(ctx);
            let combined = with_augments(&children, &node_path, &schema_path, augments_in, ctx);
            let all_children = combined.as_deref().unwrap_or(children.as_slice());
            emit_children(
                all_children,
                &node_path,
                augments_in,
                module,
                ctx,
                child_prefix,
                eff_config,
                &[],
                0,
                child_depth,
                out,
            )?;
        }
        SchemaNodeKind::Leaf {
            type_stmt,
            mandatory,
            ..
        } => {
            let is_key = list_keys.iter().any(|k| k == &node.name);
            let suffix = if *mandatory || is_key { "" } else { "?" };
            let type_str = leaf_type_display(type_stmt, &module.prefix);
            let name_col = mk_name(&format!("{}{}", dname, suffix), width);
            write!(out, "{}{}--{} {}{}", prefix, status, rw, name_col, type_str)?;
            writeln!(out)?;
        }
        SchemaNodeKind::LeafList {
            type_stmt,
            min_elements: _,
            ..
        } => {
            let suffix = "*";
            let type_str = leaf_type_display(type_stmt, &module.prefix);
            let name_col = mk_name(&format!("{}{}", dname, suffix), width);
            write!(out, "{}{}--{} {}{}", prefix, status, rw, name_col, type_str)?;
            writeln!(out)?;
        }
        SchemaNodeKind::List { key, .. } => {
            let key_str = format!(" [{}]", key.join(" "));
            writeln!(out, "{}{}--{} {}*{}", prefix, status, rw, dname, key_str)?;
            let children = node.children(ctx);
            let combined = with_augments(&children, &node_path, &schema_path, augments_in, ctx);
            let all_children = combined.as_deref().unwrap_or(children.as_slice());
            emit_children(
                all_children,
                &node_path,
                augments_in,
                module,
                ctx,
                child_prefix,
                eff_config,
                key,
                0,
                child_depth,
                out,
            )?;
        }
        SchemaNodeKind::Choice { mandatory, .. } => {
            let opt = if *mandatory { "" } else { "?" };
            writeln!(out, "{}{}--{} ({}){}", prefix, status, rw, dname, opt)?;
            let child_w = width.saturating_sub(3);
            let cases = node.children(ctx);
            emit_children(
                &cases,
                &node_path,
                augments_in,
                module,
                ctx,
                child_prefix,
                eff_config,
                &[],
                child_w,
                child_depth,
                out,
            )?;
        }
        SchemaNodeKind::Case { .. } => {
            writeln!(out, "{}+--:({})", prefix, node.name)?;
            let child_w = width.saturating_sub(3);
            let children = node.children(ctx);
            emit_children(
                &children,
                &node_path,
                augments_in,
                module,
                ctx,
                child_prefix,
                eff_config,
                list_keys,
                child_w,
                child_depth,
                out,
            )?;
        }
        SchemaNodeKind::Rpc { .. } | SchemaNodeKind::Action { .. } => {
            writeln!(out, "{}+---x {}", prefix, dname)?;
            let input = node.input_children(ctx);
            let output = node.output_children(ctx);
            let has_out = !output.is_empty();
            if !input.is_empty() {
                let inp_cont = if has_out {
                    format!("{}|  ", child_prefix)
                } else {
                    format!("{}   ", child_prefix)
                };
                writeln!(out, "{}+--ro input", child_prefix)?;
                emit_children(
                    &input,
                    &node_path,
                    augments_in,
                    module,
                    ctx,
                    &inp_cont,
                    None,
                    &[],
                    0,
                    child_depth,
                    out,
                )?;
            }
            if !output.is_empty() {
                let out_cont = format!("{}   ", child_prefix);
                writeln!(out, "{}+--ro output", child_prefix)?;
                emit_children(
                    &output,
                    &node_path,
                    augments_in,
                    module,
                    ctx,
                    &out_cont,
                    Some(false),
                    &[],
                    0,
                    child_depth,
                    out,
                )?;
            }
        }
        SchemaNodeKind::Notification { .. } => {
            writeln!(out, "{}+---n {}", prefix, dname)?;
            let children = node.children(ctx);
            emit_children(
                &children,
                &node_path,
                augments_in,
                module,
                ctx,
                child_prefix,
                eff_config,
                &[],
                0,
                child_depth,
                out,
            )?;
        }
        SchemaNodeKind::AnyXml { mandatory, .. } | SchemaNodeKind::AnyData { mandatory, .. } => {
            let suffix = if *mandatory { "" } else { "?" };
            let kw = match &node.kind {
                SchemaNodeKind::AnyXml { .. } => "anyxml",
                _ => "anydata",
            };
            let name_col = mk_name(&format!("{}{}", dname, suffix), width);
            write!(out, "{}{}--{} {}   <{}>", prefix, status, rw, name_col, kw)?;
            writeln!(out)?;
        }
        SchemaNodeKind::Uses { .. } => unreachable!("uses nodes must be expanded before rendering"),
    }
    Ok(())
}

fn rw_from_config(config: Option<bool>) -> &'static str {
    match config {
        Some(false) => "ro",
        _ => "rw",
    }
}

fn leaf_type_display(type_stmt: &yangest_core::ast::Stmt, module_prefix: &str) -> String {
    use yangest_core::ast::BuiltInKeyword;
    if type_stmt.arg.as_deref() == Some("leafref") {
        let path = type_stmt
            .substmts
            .iter()
            .find(|s| {
                matches!(
                    s.keyword,
                    yangest_core::ast::Keyword::BuiltIn(BuiltInKeyword::Path)
                )
            })
            .and_then(|s| s.arg.as_deref())
            .unwrap_or("?");
        format!("-> {}", normalize_leafref_path(path, module_prefix))
    } else {
        type_stmt.arg.as_deref().unwrap_or("unknown").to_owned()
    }
}

fn normalize_leafref_path(path: &str, module_prefix: &str) -> String {
    let starts_with_slash = path.starts_with('/');
    let mut cur_prefix = module_prefix.to_owned();

    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut result: Vec<String> = Vec::with_capacity(parts.len());

    for part in parts {
        let tokens: Vec<&str> = part.split(':').filter(|s| !s.is_empty()).collect();
        match tokens.as_slice() {
            [p, name] if *p == cur_prefix.as_str() => {
                result.push((*name).to_owned());
            }
            [new_prefix, _name] => {
                result.push(part.to_owned());
                cur_prefix = (*new_prefix).to_owned();
            }
            _ => {
                result.push(tokens.join(""));
            }
        }
    }

    let joined = result.join("/");
    if starts_with_slash {
        format!("/{}", joined)
    } else {
        joined
    }
}

inventory::submit! {
    yangest_core::plugin::PluginRegistration { factory: || Box::new(TreePlugin::default()) }
}

