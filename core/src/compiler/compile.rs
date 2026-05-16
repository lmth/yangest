// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
use std::collections::HashMap;
use std::sync::Arc;

use crate::annindex::{AnnotationIndex, PendingAnnotation};
use crate::ast::{BuiltInKeyword, ErrorCode, Keyword, Level, ModuleKey, Pos, Stmt, YError};
use crate::devindex::{DeviationIndex, PendingDeviation};
use crate::grammar::GrammarRegistry;

use super::expansion::attach_schema_path;
use super::{
    AugmentEntry, CompiledModule, ExpansionCtx, ExtensionInstance, Feature, Grouping, Identity,
    IfFeatureExpr, LocalAugmentEntry, ModuleRegistry, MustExpr, NodeOverlay, NodeOverlayMap,
    OrderedBy, PathStep, PrefixMap, SchemaNode, SchemaNodeKind, SchemaPath, Status, Typedef,
    UsesOverlay, WhenExpr, YangVersion,
};
use indexmap::IndexMap;

struct NodeCommon {
    name: String,
    pos: Pos,
    status: Status,
    config: Option<bool>,
    when: Vec<WhenExpr>,
    if_features: Vec<IfFeatureExpr>,
    description: Option<String>,
    reference: Option<String>,
    extensions: Vec<ExtensionInstance>,
}

struct IfFeatureParser<'a> {
    tokens: &'a [IfFeatureToken],
    idx: usize,
    own_prefix: &'a str,
    own_module_name: &'a str,
    prefix_map: &'a PrefixMap,
    ignore_unknown: bool,
    module_errors: &'a mut Vec<YError>,
    pos: &'a Pos,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IfFeatureToken {
    Name(String, Option<String>),
    Not,
    And,
    Or,
    LParen,
    RParen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutationMode {
    Add,
    Replace,
    Delete,
}

pub fn compile_module(
    key: &ModuleKey,
    stmt: Stmt,
    registry: &ModuleRegistry,
    dev_index: &DeviationIndex,
    ann_index: &AnnotationIndex,
) -> CompiledModule {
    let mut module_errors = Vec::new();

    let is_submodule = stmt.keyword.is_builtin(BuiltInKeyword::Submodule);

    if !stmt.keyword.is_builtin(BuiltInKeyword::Module) && !is_submodule {
        emit_error(
            &mut module_errors,
            stmt.pos.clone(),
            ErrorCode::GrammarUnexpectedStatement,
            "expected module or submodule statement",
        );
    }

    let yang_version = parse_yang_version(&stmt, &mut module_errors);

    // Submodules use belongs-to/prefix instead of namespace/prefix.
    let namespace = if is_submodule {
        String::new()
    } else {
        required_substmt_arg(
            &stmt,
            BuiltInKeyword::Namespace,
            &mut module_errors,
            "module missing required namespace statement",
        )
        .unwrap_or_default()
    };
    let prefix = if is_submodule {
        stmt.get_substmt(BuiltInKeyword::BelongsTo)
            .and_then(|bt| bt.get_substmt(BuiltInKeyword::Prefix))
            .and_then(|p| p.arg.clone())
            .unwrap_or_else(|| key.name.clone())
    } else {
        required_substmt_arg(
            &stmt,
            BuiltInKeyword::Prefix,
            &mut module_errors,
            "module missing required prefix statement",
        )
        .unwrap_or_else(|| key.name.clone())
    };

    // Collect submodule names from include statements (used by the depend plugin).
    let includes: Vec<String> = stmt
        .get_substmts(BuiltInKeyword::Include)
        .filter_map(|s| s.arg.clone())
        .collect();

    // Collect ALL declared import module names (regardless of resolution),
    // preserving declaration order.  Used by the depend plugin so it can
    // list dependencies even when the imported modules were not provided.
    let imports: Vec<String> = stmt
        .get_substmts(BuiltInKeyword::Import)
        .filter_map(|s| s.arg.clone())
        .collect();

    let prefix_map = build_prefix_map(&stmt, registry, &mut module_errors);
    let ignore_unknown_features = registry.flags.ignore_unknown_features;
    let mut typedefs = collect_typedefs(
        &stmt,
        yang_version,
        &prefix,
        &prefix_map,
        &mut module_errors,
    );
    let mut groupings =
        collect_groupings(&stmt, &prefix, &prefix_map, &key.name, &mut module_errors);
    let mut features = collect_features(
        &stmt,
        &prefix,
        &key.name,
        &prefix_map,
        &mut module_errors,
        ignore_unknown_features,
    );
    let mut identities = collect_identities(
        &stmt,
        &prefix,
        &key.name,
        &prefix_map,
        &mut module_errors,
        ignore_unknown_features,
    );

    // Merge definitions from included submodules (RFC 7950 §7.2.1:
    // "The definitions in the submodule are available to the including module").
    // Submodules are compiled before the parent module (depgraph adds include edges),
    // so we can look them up from the registry here.
    // Each merged grouping already carries its definer's prefix_map and own_prefix,
    // so no prefix_map merging into the parent is needed.
    for include_stmt in stmt.get_substmts(BuiltInKeyword::Include) {
        if let Some(sub_name) = include_stmt.arg.as_deref() {
            if let Some(sub_mod) = registry.resolve_import(sub_name, None) {
                for (k, v) in sub_mod.groupings.iter() {
                    groupings.entry(k.clone()).or_insert_with(|| v.clone());
                }
                for (k, v) in &sub_mod.typedefs {
                    typedefs.entry(k.clone()).or_insert_with(|| v.clone());
                }
                for (k, v) in &sub_mod.features {
                    features.entry(k.clone()).or_insert_with(|| v.clone());
                }
                for (k, v) in &sub_mod.identities {
                    identities.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
    }

    // Pre-compile grouping bodies once at compile time so that `expand_uses_lazy` can skip
    // `compile_schema_children` entirely during plugin-time expansion.
    // Each grouping's body is compiled using the grouping's own prefix/prefix_map.
    // Nested `uses` inside a grouping body will emit `SchemaNodeKind::Uses` nodes (not expanded),
    // which are then expanded lazily (and cached) on first encounter during plugin traversal.
    let grouping_children: HashMap<String, Arc<Vec<SchemaNode>>> = groupings
        .iter()
        .map(|(name, grouping)| {
            // Build the effective prefix map: ensure the own prefix resolves to its module name.
            let eff_prefix_map;
            let eff_prefix_map_ref: &PrefixMap = if !grouping
                .def_prefix_map
                .contains_key(&grouping.def_own_prefix)
            {
                eff_prefix_map = {
                    let mut m = grouping.def_prefix_map.clone();
                    m.insert(
                        grouping.def_own_prefix.clone(),
                        grouping.definer_module_name.clone(),
                    );
                    m
                };
                &eff_prefix_map
            } else {
                &grouping.def_prefix_map
            };

            // `instantiate_stmt_for_uses` gives each child a new position that points back
            // to the grouping definition, matching the behaviour at expansion time.
            let body_stmts: Vec<Stmt> = grouping
                .stmt
                .substmts
                .iter()
                .map(|child| instantiate_stmt_for_uses(child, &grouping.stmt.pos))
                .collect();

            let compiled = compile_schema_children(
                &body_stmts,
                key,
                yang_version,
                &grouping.def_own_prefix,
                eff_prefix_map_ref,
                registry,
                &groupings,
                &mut Vec::new(), // ignore grouping-body parse errors here
            );
            (name.clone(), Arc::new(compiled))
        })
        .collect();

    let mut children = compile_schema_children(
        &stmt.substmts,
        key,
        yang_version,
        &prefix,
        &prefix_map,
        registry,
        &groupings,
        &mut module_errors,
    );

    let all_augments = collect_augments(
        &stmt,
        key,
        yang_version,
        &prefix,
        &prefix_map,
        registry,
        &groupings,
        &mut module_errors,
    );

    // Partition augments into self-augments (targeting own nodes) and external ones.
    // Self-augments must be inlined into `children` before deviation application so
    // that deviations can target nodes introduced by self-augments.
    let mut augments = Vec::new();
    for aug in all_augments {
        let is_self = aug
            .target_path
            .first()
            .map(|step| step.prefix.as_deref().map_or(true, |p| p == prefix))
            .unwrap_or(false);
        if is_self {
            inline_augment_into(&aug.target_path, aug.nodes, &mut children);
        } else {
            augments.push(aug);
        }
    }

    let mut overlay = NodeOverlayMap::new();
    apply_deviations(
        key,
        &mut children,
        registry,
        dev_index,
        &mut module_errors,
        &mut overlay,
    );
    apply_annotations(
        key,
        &mut children,
        ann_index,
        &mut module_errors,
        &mut overlay,
    );

    // Collect module-level extension instances (direct sub-stmts of the module statement).
    // We resolve the own prefix to the module name here, which compile_node_common cannot do
    // (it doesn't know the module name, only the prefix string).
    let extensions = collect_module_extensions(
        &stmt,
        &prefix,
        &prefix_map,
        &registry.grammar,
        &key.name,
        &mut module_errors,
    );

    CompiledModule {
        key: key.clone(),
        yang_version,
        namespace,
        prefix,
        prefix_map,
        typedefs,
        groupings: Arc::new(groupings),
        features,
        identities,
        children,
        augments,
        overlay,
        errors: module_errors,
        stmt,
        pmap: HashMap::new(),
        extensions,
        imports,
        includes,
        source_path: None,
        grouping_children,
    }
}

fn parse_yang_version(stmt: &Stmt, module_errors: &mut Vec<YError>) -> YangVersion {
    match stmt.get_substmt(BuiltInKeyword::YangVersion) {
        None => YangVersion::V1,
        Some(version_stmt) => match version_stmt.arg.as_deref() {
            Some("1") => YangVersion::V1,
            Some("1.1") => YangVersion::V11,
            Some(other) => {
                emit_error(
                    module_errors,
                    version_stmt.pos.clone(),
                    ErrorCode::GrammarBadYangVersion,
                    format!("unsupported yang-version '{other}'"),
                );
                YangVersion::V1
            }
            None => {
                emit_error(
                    module_errors,
                    version_stmt.pos.clone(),
                    ErrorCode::GrammarMissingRequired,
                    "yang-version requires an argument",
                );
                YangVersion::V1
            }
        },
    }
}

fn build_prefix_map(
    stmt: &Stmt,
    registry: &ModuleRegistry,
    module_errors: &mut Vec<YError>,
) -> PrefixMap {
    let mut prefix_map = PrefixMap::new();

    for import in stmt.get_substmts(BuiltInKeyword::Import) {
        let module_name = import.arg.as_deref().unwrap_or("");
        if module_name.is_empty() {
            emit_error(
                module_errors,
                import.pos.clone(),
                ErrorCode::GrammarMissingRequired,
                "import requires a module name argument",
            );
            continue;
        }

        let import_prefix = match required_substmt_arg(
            import,
            BuiltInKeyword::Prefix,
            module_errors,
            "import missing required prefix statement",
        ) {
            Some(prefix) => prefix,
            None => continue,
        };

        let revision = import
            .get_substmt(BuiltInKeyword::RevisionDate)
            .and_then(|s| s.arg.as_deref());
        match registry.resolve_import(module_name, revision) {
            Some(module) => {
                prefix_map.insert(import_prefix, module.key.name.clone());
            }
            None => emit_error(
                module_errors,
                import.pos.clone(),
                ErrorCode::ModuleNotFound,
                format!("imported module '{module_name}' not found in registry"),
            ),
        }
    }

    prefix_map
}

fn collect_typedefs(
    stmt: &Stmt,
    yang_version: YangVersion,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    module_errors: &mut Vec<YError>,
) -> IndexMap<String, Typedef> {
    let mut typedefs = IndexMap::new();

    for typedef in stmt.get_substmts(BuiltInKeyword::Typedef) {
        let name = required_stmt_name(typedef, module_errors).unwrap_or_default();
        let type_stmt = typedef
            .get_substmt(BuiltInKeyword::Type)
            .cloned()
            .unwrap_or_else(|| missing_type_stmt(typedef, module_errors));
        validate_type_stmt(
            &type_stmt,
            yang_version,
            own_prefix,
            prefix_map,
            module_errors,
        );
        typedefs.insert(
            name.clone(),
            Typedef {
                name,
                type_stmt,
                units: opt_substmt_arg(typedef, BuiltInKeyword::Units),
                default: opt_substmt_arg(typedef, BuiltInKeyword::Default),
                status: parse_status(typedef, module_errors),
                description: opt_substmt_arg(typedef, BuiltInKeyword::Description),
            },
        );
    }

    typedefs
}

fn collect_groupings(
    stmt: &Stmt,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    module_name: &str,
    module_errors: &mut Vec<YError>,
) -> IndexMap<String, Grouping> {
    collect_groupings_from_stmts(
        stmt.get_substmts(BuiltInKeyword::Grouping),
        own_prefix,
        prefix_map,
        module_name,
        module_errors,
    )
}

fn collect_groupings_from_stmts<'a>(
    iter: impl Iterator<Item = &'a Stmt>,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    module_name: &str,
    module_errors: &mut Vec<YError>,
) -> IndexMap<String, Grouping> {
    let mut groupings = IndexMap::new();

    for grouping in iter {
        let name = required_stmt_name(grouping, module_errors).unwrap_or_default();
        groupings.insert(
            name.clone(),
            Grouping {
                name,
                status: parse_status(grouping, module_errors),
                description: opt_substmt_arg(grouping, BuiltInKeyword::Description),
                stmt: grouping.clone(),
                def_prefix_map: prefix_map.clone(),
                def_own_prefix: own_prefix.to_string(),
                definer_module_name: module_name.to_string(),
            },
        );
    }

    groupings
}

fn collect_features(
    stmt: &Stmt,
    own_prefix: &str,
    own_module_name: &str,
    prefix_map: &PrefixMap,
    module_errors: &mut Vec<YError>,
    ignore_unknown: bool,
) -> IndexMap<String, Feature> {
    let mut features = IndexMap::new();

    for feature_stmt in stmt.get_substmts(BuiltInKeyword::Feature) {
        let name = required_stmt_name(feature_stmt, module_errors).unwrap_or_default();
        features.insert(
            name.clone(),
            Feature {
                name,
                if_features: collect_if_features(
                    feature_stmt,
                    own_prefix,
                    own_module_name,
                    prefix_map,
                    module_errors,
                    ignore_unknown,
                ),
                status: parse_status(feature_stmt, module_errors),
                description: opt_substmt_arg(feature_stmt, BuiltInKeyword::Description),
            },
        );
    }

    features
}

fn collect_identities(
    stmt: &Stmt,
    own_prefix: &str,
    own_module_name: &str,
    prefix_map: &PrefixMap,
    module_errors: &mut Vec<YError>,
    ignore_unknown: bool,
) -> IndexMap<String, Identity> {
    let mut identities = IndexMap::new();

    for identity_stmt in stmt.get_substmts(BuiltInKeyword::Identity) {
        let name = required_stmt_name(identity_stmt, module_errors).unwrap_or_default();
        let mut bases = Vec::new();
        for base in identity_stmt.get_substmts(BuiltInKeyword::Base) {
            let (prefix, base_name) = split_prefixed_name(base.arg.as_deref().unwrap_or(""));
            if let Some(prefix_name) = &prefix {
                validate_known_prefix(
                    prefix_name,
                    own_prefix,
                    prefix_map,
                    &base.pos,
                    module_errors,
                );
            }
            if base_name.is_empty() {
                emit_error(
                    module_errors,
                    base.pos.clone(),
                    ErrorCode::GrammarMissingRequired,
                    "base requires an identifier",
                );
            }
            bases.push((prefix, base_name));
        }

        identities.insert(
            name.clone(),
            Identity {
                name,
                bases,
                if_features: collect_if_features(
                    identity_stmt,
                    own_prefix,
                    own_module_name,
                    prefix_map,
                    module_errors,
                    ignore_unknown,
                ),
                status: parse_status(identity_stmt, module_errors),
                description: opt_substmt_arg(identity_stmt, BuiltInKeyword::Description),
            },
        );
    }

    identities
}

fn collect_augments(
    stmt: &Stmt,
    key: &ModuleKey,
    yang_version: YangVersion,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    registry: &ModuleRegistry,
    local_groupings: &IndexMap<String, Grouping>,
    module_errors: &mut Vec<YError>,
) -> Vec<AugmentEntry> {
    let mut augments = Vec::new();
    let ignore_unknown = registry.flags.ignore_unknown_features;

    for augment in stmt.get_substmts(BuiltInKeyword::Augment) {
        let Some(target_path) = parse_absolute_schema_path(
            augment.arg.as_deref().unwrap_or(""),
            own_prefix,
            prefix_map,
            &augment.pos,
            module_errors,
        ) else {
            continue;
        };

        let nodes = compile_schema_children(
            &augment.substmts,
            key,
            yang_version,
            own_prefix,
            prefix_map,
            registry,
            local_groupings,
            module_errors,
        );

        augments.push(AugmentEntry {
            target_path,
            nodes,
            when: collect_when_exprs(augment, own_prefix, prefix_map, module_errors),
            if_features: collect_if_features(
                augment,
                own_prefix,
                &key.name,
                prefix_map,
                module_errors,
                ignore_unknown,
            ),
        });
    }

    augments
}

/// Inline a self-augment's nodes into the target node's children list.
///
/// `target_path` is the parsed augment path (prefix already stripped/resolved).
/// The function walks into `children` following the path and appends `nodes`
/// to the first matching container/list/etc. If the target node is not found
/// the nodes are silently dropped (augment target errors are caught elsewhere).
fn inline_augment_into(
    target_path: &[PathStep],
    nodes: Vec<SchemaNode>,
    children: &mut Vec<SchemaNode>,
) {
    if let Some(target) = find_node_mut(children, target_path) {
        match &mut target.kind {
            SchemaNodeKind::Container { children: c, .. }
            | SchemaNodeKind::List { children: c, .. }
            | SchemaNodeKind::Case { children: c }
            | SchemaNodeKind::Notification { children: c, .. } => c.extend(nodes),
            SchemaNodeKind::Choice { cases, .. } => cases.extend(nodes),
            _ => {}
        }
    }
}

fn compile_schema_children(
    stmts: &[Stmt],
    key: &ModuleKey,
    yang_version: YangVersion,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    registry: &ModuleRegistry,
    local_groupings: &IndexMap<String, Grouping>,
    module_errors: &mut Vec<YError>,
) -> Vec<SchemaNode> {
    let mut nodes = Vec::new();

    // RFC 7950 §7.13: groupings can be defined at any schema level. Collect any
    // inline grouping definitions from this context so that `uses` statements at
    // the same or deeper level can reference them (e.g. a grouping nested inside
    // another grouping's body).
    let inline_groupings = collect_groupings_from_stmts(
        stmts
            .iter()
            .filter(|s| s.keyword.is_builtin(BuiltInKeyword::Grouping)),
        own_prefix,
        prefix_map,
        &key.name,
        module_errors,
    );
    let merged;
    let effective_groupings: &IndexMap<String, Grouping> = if inline_groupings.is_empty() {
        local_groupings
    } else {
        merged = {
            let mut m = local_groupings.clone();
            m.extend(inline_groupings);
            m
        };
        &merged
    };

    for stmt in stmts {
        match builtin_keyword(stmt) {
            Some(BuiltInKeyword::Container)
            | Some(BuiltInKeyword::Leaf)
            | Some(BuiltInKeyword::LeafList)
            | Some(BuiltInKeyword::List)
            | Some(BuiltInKeyword::Choice)
            | Some(BuiltInKeyword::Case)
            | Some(BuiltInKeyword::Rpc)
            | Some(BuiltInKeyword::Action)
            | Some(BuiltInKeyword::Notification)
            | Some(BuiltInKeyword::AnyXml)
            | Some(BuiltInKeyword::AnyData) => {
                if let Some(node) = compile_schema_node(
                    stmt,
                    key,
                    yang_version,
                    own_prefix,
                    prefix_map,
                    registry,
                    effective_groupings,
                    module_errors,
                ) {
                    nodes.push(node);
                }
            }
            Some(BuiltInKeyword::Uses) => {
                if let Some(node) = compile_uses_node(
                    stmt,
                    key,
                    yang_version,
                    own_prefix,
                    prefix_map,
                    registry,
                    effective_groupings,
                    module_errors,
                ) {
                    nodes.push(node);
                }
            }
            _ => {}
        }
    }

    nodes
}

fn compile_uses_node(
    uses_stmt: &Stmt,
    key: &ModuleKey,
    yang_version: YangVersion,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    registry: &ModuleRegistry,
    local_groupings: &IndexMap<String, Grouping>,
    module_errors: &mut Vec<YError>,
) -> Option<SchemaNode> {
    let ignore_unknown = registry.flags.ignore_unknown_features;
    let uses_arg = uses_stmt.arg.as_deref().unwrap_or("");
    let (grouping_prefix, grouping_name) = split_prefixed_name(uses_arg);
    let grouping = resolve_grouping(
        grouping_prefix.as_deref(),
        &grouping_name,
        own_prefix,
        prefix_map,
        registry,
        local_groupings,
        &uses_stmt.pos,
        module_errors,
    )?;

    let source_module_name = grouping_prefix
        .as_deref()
        .filter(|prefix| *prefix != own_prefix)
        .and_then(|prefix| prefix_map.get(prefix).cloned());

    let mut local_augments = Vec::new();
    for augment in uses_stmt.get_substmts(BuiltInKeyword::Augment) {
        let Some(target_path) = parse_relative_schema_path(
            augment.arg.as_deref().unwrap_or(""),
            &augment.pos,
            module_errors,
        ) else {
            continue;
        };
        let nodes = compile_schema_children(
            &augment.substmts,
            key,
            yang_version,
            own_prefix,
            prefix_map,
            registry,
            local_groupings,
            module_errors,
        );
        local_augments.push(LocalAugmentEntry {
            target_path: target_path.into_iter().map(|step| step.name).collect(),
            nodes,
            when: collect_when_exprs(augment, own_prefix, prefix_map, module_errors),
            if_features: collect_if_features(
                augment,
                own_prefix,
                &key.name,
                prefix_map,
                module_errors,
                ignore_unknown,
            ),
        });
    }

    Some(SchemaNode {
        name: "__uses__".to_string(),
        module_name: key.name.clone(),
        module_prefix: own_prefix.to_string(),
        pos: uses_stmt.pos.clone(),
        status: Status::Current,
        config: None,
        when: Vec::new(),
        if_features: Vec::new(),
        description: None,
        reference: None,
        extensions: Vec::new(),
        kind: SchemaNodeKind::Uses {
            grouping: Arc::new(grouping),
            source_module_name,
            overlay: UsesOverlay {
                refine_stmts: uses_stmt
                    .get_substmts(BuiltInKeyword::Refine)
                    .cloned()
                    .collect(),
                local_augments,
                when: collect_when_exprs(uses_stmt, own_prefix, prefix_map, module_errors),
                if_features: collect_if_features(
                    uses_stmt,
                    own_prefix,
                    &key.name,
                    prefix_map,
                    module_errors,
                    ignore_unknown,
                ),
            },
        },
        pmap: HashMap::new(),
    })
}

fn compile_schema_node(
    stmt: &Stmt,
    key: &ModuleKey,
    yang_version: YangVersion,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    registry: &ModuleRegistry,
    local_groupings: &IndexMap<String, Grouping>,
    module_errors: &mut Vec<YError>,
) -> Option<SchemaNode> {
    let common = compile_node_common(
        stmt,
        own_prefix,
        &key.name,
        prefix_map,
        &registry.grammar,
        module_errors,
        registry.flags.ignore_unknown_features,
    );

    let kind = match builtin_keyword(stmt)? {
        BuiltInKeyword::Container => SchemaNodeKind::Container {
            presence: opt_substmt_arg(stmt, BuiltInKeyword::Presence),
            children: compile_schema_children(
                &stmt.substmts,
                key,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
            ),
            musts: collect_must_exprs(stmt, own_prefix, prefix_map, module_errors),
        },
        BuiltInKeyword::Leaf => SchemaNodeKind::Leaf {
            type_stmt: compile_type_stmt(stmt, yang_version, own_prefix, prefix_map, module_errors),
            units: opt_substmt_arg(stmt, BuiltInKeyword::Units),
            default: opt_substmt_arg(stmt, BuiltInKeyword::Default),
            mandatory: opt_bool_substmt(stmt, BuiltInKeyword::Mandatory, module_errors)
                .unwrap_or(false),
            musts: collect_must_exprs(stmt, own_prefix, prefix_map, module_errors),
        },
        BuiltInKeyword::LeafList => SchemaNodeKind::LeafList {
            type_stmt: compile_type_stmt(stmt, yang_version, own_prefix, prefix_map, module_errors),
            units: opt_substmt_arg(stmt, BuiltInKeyword::Units),
            default: stmt
                .get_substmts(BuiltInKeyword::Default)
                .filter_map(|default| default.arg.clone())
                .collect(),
            min_elements: opt_u64_substmt(stmt, BuiltInKeyword::MinElements, module_errors)
                .unwrap_or(0),
            max_elements: opt_max_elements(stmt, module_errors),
            ordered_by: opt_ordered_by(stmt, module_errors).unwrap_or(OrderedBy::System),
            musts: collect_must_exprs(stmt, own_prefix, prefix_map, module_errors),
        },
        BuiltInKeyword::List => SchemaNodeKind::List {
            key: parse_key_stmt(stmt.get_substmt(BuiltInKeyword::Key)),
            unique: stmt
                .get_substmts(BuiltInKeyword::Unique)
                .filter_map(|unique| unique.arg.clone())
                .collect(),
            children: compile_schema_children(
                &stmt.substmts,
                key,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
            ),
            min_elements: opt_u64_substmt(stmt, BuiltInKeyword::MinElements, module_errors)
                .unwrap_or(0),
            max_elements: opt_max_elements(stmt, module_errors),
            ordered_by: opt_ordered_by(stmt, module_errors).unwrap_or(OrderedBy::System),
            musts: collect_must_exprs(stmt, own_prefix, prefix_map, module_errors),
        },
        BuiltInKeyword::Choice => SchemaNodeKind::Choice {
            default: opt_substmt_arg(stmt, BuiltInKeyword::Default),
            mandatory: opt_bool_substmt(stmt, BuiltInKeyword::Mandatory, module_errors)
                .unwrap_or(false),
            cases: compile_choice_cases(
                stmt,
                key,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
            ),
        },
        BuiltInKeyword::Case => SchemaNodeKind::Case {
            children: compile_schema_children(
                &stmt.substmts,
                key,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
            ),
        },
        BuiltInKeyword::Rpc => SchemaNodeKind::Rpc {
            input: compile_io_block(
                stmt.get_substmt(BuiltInKeyword::Input),
                key,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
            ),
            output: compile_io_block(
                stmt.get_substmt(BuiltInKeyword::Output),
                key,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
            ),
            musts: collect_must_exprs(stmt, own_prefix, prefix_map, module_errors),
        },
        BuiltInKeyword::Action => SchemaNodeKind::Action {
            input: compile_io_block(
                stmt.get_substmt(BuiltInKeyword::Input),
                key,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
            ),
            output: compile_io_block(
                stmt.get_substmt(BuiltInKeyword::Output),
                key,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
            ),
        },
        BuiltInKeyword::Notification => SchemaNodeKind::Notification {
            children: compile_schema_children(
                &stmt.substmts,
                key,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
            ),
            musts: collect_must_exprs(stmt, own_prefix, prefix_map, module_errors),
        },
        BuiltInKeyword::AnyXml => SchemaNodeKind::AnyXml {
            mandatory: opt_bool_substmt(stmt, BuiltInKeyword::Mandatory, module_errors)
                .unwrap_or(false),
            musts: collect_must_exprs(stmt, own_prefix, prefix_map, module_errors),
        },
        BuiltInKeyword::AnyData => SchemaNodeKind::AnyData {
            mandatory: opt_bool_substmt(stmt, BuiltInKeyword::Mandatory, module_errors)
                .unwrap_or(false),
            musts: collect_must_exprs(stmt, own_prefix, prefix_map, module_errors),
        },
        _ => return None,
    };

    Some(SchemaNode {
        name: common.name,
        module_name: key.name.clone(),
        module_prefix: own_prefix.to_string(),
        pos: common.pos,
        status: common.status,
        config: common.config,
        when: common.when,
        if_features: common.if_features,
        description: common.description,
        reference: common.reference,
        extensions: common.extensions,
        kind,
        pmap: HashMap::new(),
    })
}

fn compile_choice_cases(
    stmt: &Stmt,
    key: &ModuleKey,
    yang_version: YangVersion,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    registry: &ModuleRegistry,
    local_groupings: &IndexMap<String, Grouping>,
    module_errors: &mut Vec<YError>,
) -> Vec<SchemaNode> {
    let mut cases = Vec::new();

    for sub in &stmt.substmts {
        match builtin_keyword(sub) {
            Some(BuiltInKeyword::Case) => {
                if let Some(case_node) = compile_schema_node(
                    sub,
                    key,
                    yang_version,
                    own_prefix,
                    prefix_map,
                    registry,
                    local_groupings,
                    module_errors,
                ) {
                    cases.push(case_node);
                }
            }
            Some(BuiltInKeyword::Container)
            | Some(BuiltInKeyword::Leaf)
            | Some(BuiltInKeyword::LeafList)
            | Some(BuiltInKeyword::List)
            | Some(BuiltInKeyword::Choice)
            | Some(BuiltInKeyword::AnyXml)
            | Some(BuiltInKeyword::AnyData)
            | Some(BuiltInKeyword::Uses) => {
                let children: Vec<SchemaNode> = match builtin_keyword(sub) {
                    Some(BuiltInKeyword::Uses) => compile_uses_node(
                        sub,
                        key,
                        yang_version,
                        own_prefix,
                        prefix_map,
                        registry,
                        local_groupings,
                        module_errors,
                    )
                    .into_iter()
                    .collect(),
                    _ => compile_schema_node(
                        sub,
                        key,
                        yang_version,
                        own_prefix,
                        prefix_map,
                        registry,
                        local_groupings,
                        module_errors,
                    )
                    .into_iter()
                    .collect(),
                };

                let case_name = sub.arg.clone().unwrap_or_else(|| "__case__".to_string());
                cases.push(SchemaNode {
                    name: case_name,
                    module_name: key.name.clone(),
                    module_prefix: own_prefix.to_string(),
                    pos: sub.pos.clone(),
                    status: Status::Current,
                    config: None,
                    when: Vec::new(),
                    if_features: Vec::new(),
                    description: None,
                    reference: None,
                    extensions: Vec::new(),
                    kind: SchemaNodeKind::Case { children },
                    pmap: HashMap::new(),
                });
            }
            _ => {}
        }
    }

    cases
}

fn compile_io_block(
    stmt: Option<&Stmt>,
    key: &ModuleKey,
    yang_version: YangVersion,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    registry: &ModuleRegistry,
    local_groupings: &IndexMap<String, Grouping>,
    module_errors: &mut Vec<YError>,
) -> Vec<SchemaNode> {
    stmt.map(|io| {
        compile_schema_children(
            &io.substmts,
            key,
            yang_version,
            own_prefix,
            prefix_map,
            registry,
            local_groupings,
            module_errors,
        )
    })
    .unwrap_or_default()
}

fn compile_node_common(
    stmt: &Stmt,
    own_prefix: &str,
    own_module_name: &str,
    prefix_map: &PrefixMap,
    grammar: &GrammarRegistry,
    module_errors: &mut Vec<YError>,
    ignore_unknown: bool,
) -> NodeCommon {
    let extensions =
        collect_extension_instances(stmt, own_prefix, prefix_map, grammar, module_errors);
    NodeCommon {
        name: required_stmt_name(stmt, module_errors).unwrap_or_default(),
        pos: stmt.pos.clone(),
        status: parse_status(stmt, module_errors),
        config: opt_bool_substmt(stmt, BuiltInKeyword::Config, module_errors),
        when: collect_when_exprs(stmt, own_prefix, prefix_map, module_errors),
        if_features: collect_if_features(
            stmt,
            own_prefix,
            own_module_name,
            prefix_map,
            module_errors,
            ignore_unknown,
        ),
        description: opt_substmt_arg(stmt, BuiltInKeyword::Description),
        reference: opt_substmt_arg(stmt, BuiltInKeyword::Reference),
        extensions,
    }
}

/// Collect and validate extension sub-statements from `stmt` into `ExtensionInstance` values.
///
/// Each sub-statement whose keyword is an extension (prefixed or resolved) is resolved to its
/// module name via `prefix_map` and stored as an `ExtensionInstance`.  If the extension has a
/// registered grammar rule, that rule is used to validate the argument and sub-statements.
fn collect_extension_instances(
    stmt: &Stmt,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    grammar: &GrammarRegistry,
    module_errors: &mut Vec<YError>,
) -> Vec<ExtensionInstance> {
    let mut result = Vec::new();

    for sub in &stmt.substmts {
        let (module, name) = match &sub.keyword {
            Keyword::ExtensionPrefixed { prefix, name } => {
                let module = if prefix == own_prefix {
                    // Own prefix → use the module's own name, which is not in prefix_map.
                    // We cannot know the module name here without extra context, so fall back
                    // to the prefix string.  Callers that know the module name (compile_module)
                    // re-collect module-level extensions with the correct name.
                    prefix.clone()
                } else {
                    prefix_map
                        .get(prefix.as_str())
                        .cloned()
                        .unwrap_or_else(|| prefix.clone())
                };
                (module, name.clone())
            }
            Keyword::Extension { module, name } => (module.clone(), name.clone()),
            _ => continue,
        };

        // Validate against the declared grammar rule, if one is registered.
        if let Some(rule) = grammar.get(&module, &name) {
            validate_extension_instance(sub, rule, own_prefix, &module, prefix_map, module_errors);
        }

        result.push(ExtensionInstance {
            module,
            name,
            arg: sub.arg.clone(),
            substmts: sub.substmts.clone(),
        });
    }

    result
}

///
/// Returns `None` for built-in keywords (handled separately) and for keywords
/// that cannot be parsed as extensions.
fn resolve_kw<'a>(
    keyword: &'a Keyword,
    own_prefix: &str,
    own_module: &'a str,
    prefix_map: &'a PrefixMap,
) -> Option<(&'a str, &'a str)> {
    match keyword {
        Keyword::Extension { module, name } => Some((module.as_str(), name.as_str())),
        Keyword::ExtensionPrefixed { prefix, name } => {
            let module = if prefix == own_prefix {
                own_module
            } else {
                prefix_map.get(prefix.as_str()).map(|s| s.as_str()).unwrap_or(prefix.as_str())
            };
            Some((module, name.as_str()))
        }
        _ => None,
    }
}

/// Validate one extension instance against its declared grammar rule.
fn validate_extension_instance(
    stmt: &Stmt,
    rule: &crate::grammar::ExtensionGrammar,
    own_prefix: &str,
    own_module: &str,
    prefix_map: &PrefixMap,
    module_errors: &mut Vec<YError>,
) {
    use crate::grammar::{ArgType, Cardinality, GrammarKeyword};

    // Validate argument presence / absence.
    match (rule.arg, &stmt.arg) {
        (None, Some(_)) => emit_error(
            module_errors,
            stmt.pos.clone(),
            ErrorCode::GrammarUnexpectedStatement,
            &format!(
                "extension {}:{} does not accept an argument",
                rule.module, rule.name
            ),
        ),
        (Some(ArgType::YangIdentifier), Some(arg)) => {
            if !is_yang_identifier(arg) {
                emit_error(
                    module_errors,
                    stmt.pos.clone(),
                    ErrorCode::GrammarMissingRequired,
                    &format!(
                        "extension {}:{} argument must be a YANG identifier, got '{}'",
                        rule.module, rule.name, arg
                    ),
                );
            }
        }
        (Some(ArgType::Integer), Some(arg)) => {
            if arg.parse::<i64>().is_err() {
                emit_error(
                    module_errors,
                    stmt.pos.clone(),
                    ErrorCode::GrammarMissingRequired,
                    &format!(
                        "extension {}:{} argument must be an integer, got '{}'",
                        rule.module, rule.name, arg
                    ),
                );
            }
        }
        _ => {} // String / Any / None-without-arg: no further validation needed
    }

    // Validate sub-statements against grammar specs.
    for sub in &stmt.substmts {
        let known = rule.substmts.iter().any(|spec| match spec.keyword {
            GrammarKeyword::BuiltIn(kw) => sub.keyword.is_builtin(kw),
            GrammarKeyword::Extension { module, name } => {
                resolve_kw(&sub.keyword, own_prefix, own_module, prefix_map)
                    .map_or(false, |(m, n)| m == module && n == name)
            }
            GrammarKeyword::AnyExtension => resolve_kw(&sub.keyword, own_prefix, own_module, prefix_map).is_some(),
            GrammarKeyword::AnyBuiltIn => matches!(sub.keyword, Keyword::BuiltIn(_)),
        });
        if !known {
            emit_error(
                module_errors,
                sub.pos.clone(),
                ErrorCode::GrammarUnexpectedStatement,
                &format!(
                    "unexpected sub-statement '{}' in extension {}:{}",
                    sub.keyword, rule.module, rule.name
                ),
            );
        }
    }

    // Check required sub-statements are present.
    for spec in rule.substmts {
        if spec.cardinality == Cardinality::Required || spec.cardinality == Cardinality::OneOrMore {
            let present = stmt.substmts.iter().any(|sub| match spec.keyword {
                GrammarKeyword::BuiltIn(kw) => sub.keyword.is_builtin(kw),
                GrammarKeyword::Extension { module, name } => {
                    resolve_kw(&sub.keyword, own_prefix, own_module, prefix_map)
                        .map_or(false, |(m, n)| m == module && n == name)
                }
                GrammarKeyword::AnyExtension => resolve_kw(&sub.keyword, own_prefix, own_module, prefix_map).is_some(),
                GrammarKeyword::AnyBuiltIn => matches!(sub.keyword, Keyword::BuiltIn(_)),
            });
            if !present {
                let kw_label = match spec.keyword {
                    GrammarKeyword::BuiltIn(kw) => format!("{kw:?}"),
                    GrammarKeyword::Extension { module, name } => format!("{module}:{name}"),
                    GrammarKeyword::AnyExtension => "any extension".to_string(),
                    GrammarKeyword::AnyBuiltIn => "any built-in".to_string(),
                };
                emit_error(
                    module_errors,
                    stmt.pos.clone(),
                    ErrorCode::GrammarMissingRequired,
                    &format!(
                        "extension {}:{} requires sub-statement '{}'",
                        rule.module, rule.name, kw_label
                    ),
                );
            }
        }
    }
}

fn is_yang_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// Collect module-level extension instances.
///
/// Like `collect_extension_instances`, but resolves the module's own prefix to its actual
/// module name (which `compile_node_common` cannot do because it only knows the prefix string).
fn collect_module_extensions(
    stmt: &Stmt,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    grammar: &GrammarRegistry,
    own_module_name: &str,
    module_errors: &mut Vec<YError>,
) -> Vec<ExtensionInstance> {
    let mut result = Vec::new();

    for sub in &stmt.substmts {
        let (module, name) = match &sub.keyword {
            Keyword::ExtensionPrefixed { prefix, name } => {
                let module = if prefix == own_prefix {
                    own_module_name.to_string()
                } else {
                    prefix_map
                        .get(prefix.as_str())
                        .cloned()
                        .unwrap_or_else(|| prefix.clone())
                };
                (module, name.clone())
            }
            Keyword::Extension { module, name } => (module.clone(), name.clone()),
            _ => continue,
        };

        if let Some(rule) = grammar.get(&module, &name) {
            validate_extension_instance(sub, rule, own_prefix, &module, prefix_map, module_errors);
        }

        result.push(ExtensionInstance {
            module,
            name,
            arg: sub.arg.clone(),
            substmts: sub.substmts.clone(),
        });
    }

    result
}

fn compile_type_stmt(
    stmt: &Stmt,
    yang_version: YangVersion,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    module_errors: &mut Vec<YError>,
) -> Stmt {
    let type_stmt = stmt
        .get_substmt(BuiltInKeyword::Type)
        .cloned()
        .unwrap_or_else(|| missing_type_stmt(stmt, module_errors));
    validate_type_stmt(
        &type_stmt,
        yang_version,
        own_prefix,
        prefix_map,
        module_errors,
    );
    type_stmt
}

fn validate_type_stmt(
    stmt: &Stmt,
    _yang_version: YangVersion,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    module_errors: &mut Vec<YError>,
) {
    let (prefix, name) = split_prefixed_name(stmt.arg.as_deref().unwrap_or(""));
    if let Some(prefix) = prefix {
        validate_known_prefix(&prefix, own_prefix, prefix_map, &stmt.pos, module_errors);
    } else if name.is_empty() {
        emit_error(
            module_errors,
            stmt.pos.clone(),
            ErrorCode::GrammarMissingRequired,
            "type requires an argument",
        );
    }
}

fn collect_if_features(
    stmt: &Stmt,
    own_prefix: &str,
    own_module_name: &str,
    prefix_map: &PrefixMap,
    module_errors: &mut Vec<YError>,
    ignore_unknown: bool,
) -> Vec<IfFeatureExpr> {
    let mut result = Vec::new();

    for if_feature in stmt.get_substmts(BuiltInKeyword::IfFeature) {
        let Some(raw) = if_feature.arg.as_deref() else {
            emit_error(
                module_errors,
                if_feature.pos.clone(),
                ErrorCode::GrammarMissingRequired,
                "if-feature requires an argument",
            );
            continue;
        };

        if let Some(expr) = parse_if_feature_expr(
            raw,
            own_prefix,
            own_module_name,
            prefix_map,
            &if_feature.pos,
            module_errors,
            ignore_unknown,
        ) {
            result.push(expr);
        }
    }

    result
}

fn parse_if_feature_expr(
    raw: &str,
    own_prefix: &str,
    own_module_name: &str,
    prefix_map: &PrefixMap,
    pos: &Pos,
    module_errors: &mut Vec<YError>,
    ignore_unknown: bool,
) -> Option<IfFeatureExpr> {
    let tokens = tokenize_if_feature(raw, pos, module_errors)?;
    let (expr, idx, len) = {
        let mut parser = IfFeatureParser {
            tokens: &tokens,
            idx: 0,
            own_prefix,
            own_module_name,
            prefix_map,
            ignore_unknown,
            module_errors,
            pos,
        };
        let expr = parser.parse_expr()?;
        (expr, parser.idx, parser.tokens.len())
    };
    if idx != len {
        emit_error(
            module_errors,
            pos.clone(),
            ErrorCode::GrammarBadArgument,
            format!("unexpected trailing input in if-feature expression '{raw}'"),
        );
        return None;
    }
    Some(expr)
}

impl<'a> IfFeatureParser<'a> {
    fn parse_expr(&mut self) -> Option<IfFeatureExpr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Option<IfFeatureExpr> {
        let mut lhs = self.parse_and()?;
        while self.peek() == Some(&IfFeatureToken::Or) {
            self.idx += 1;
            let rhs = self.parse_and()?;
            lhs = IfFeatureExpr::Or(Box::new(lhs), Box::new(rhs));
        }
        Some(lhs)
    }

    fn parse_and(&mut self) -> Option<IfFeatureExpr> {
        let mut lhs = self.parse_unary()?;
        while self.peek() == Some(&IfFeatureToken::And) {
            self.idx += 1;
            let rhs = self.parse_unary()?;
            lhs = IfFeatureExpr::And(Box::new(lhs), Box::new(rhs));
        }
        Some(lhs)
    }

    fn parse_unary(&mut self) -> Option<IfFeatureExpr> {
        if self.peek() == Some(&IfFeatureToken::Not) {
            self.idx += 1;
            return Some(IfFeatureExpr::Not(Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Option<IfFeatureExpr> {
        let token = self.tokens.get(self.idx)?.clone();
        match token {
            IfFeatureToken::Name(name, prefix_opt) => {
                self.idx += 1;
                let resolved = match prefix_opt {
                    None => IfFeatureExpr::Name(name, None),
                    Some(prefix) => {
                        let module_name = if prefix == self.own_prefix {
                            self.own_module_name.to_string()
                        } else if let Some(module_name) = self.prefix_map.get(&prefix) {
                            module_name.clone()
                        } else {
                            if !self.ignore_unknown {
                                emit_error(
                                    self.module_errors,
                                    self.pos.clone(),
                                    ErrorCode::ModuleNotFound,
                                    format!("unknown prefix '{prefix}' in if-feature expression"),
                                );
                            }
                            String::new()
                        };
                        IfFeatureExpr::Name(name, Some(module_name))
                    }
                };
                Some(resolved)
            }
            IfFeatureToken::LParen => {
                self.idx += 1;
                let expr = self.parse_expr()?;
                if self.peek() != Some(&IfFeatureToken::RParen) {
                    return None;
                }
                self.idx += 1;
                Some(expr)
            }
            _ => None,
        }
    }

    fn peek(&self) -> Option<&IfFeatureToken> {
        self.tokens.get(self.idx)
    }
}

fn tokenize_if_feature(
    raw: &str,
    pos: &Pos,
    module_errors: &mut Vec<YError>,
) -> Option<Vec<IfFeatureToken>> {
    let chars: Vec<char> = raw.chars().collect();
    let mut idx = 0;
    let mut tokens = Vec::new();

    while idx < chars.len() {
        match chars[idx] {
            c if c.is_whitespace() => idx += 1,
            '(' => {
                tokens.push(IfFeatureToken::LParen);
                idx += 1;
            }
            ')' => {
                tokens.push(IfFeatureToken::RParen);
                idx += 1;
            }
            c if is_ident_char(c) => {
                let start = idx;
                idx += 1;
                while idx < chars.len() && is_ident_char(chars[idx]) {
                    idx += 1;
                }
                let token: String = chars[start..idx].iter().collect();
                match token.as_str() {
                    "not" => tokens.push(IfFeatureToken::Not),
                    "and" => tokens.push(IfFeatureToken::And),
                    "or" => tokens.push(IfFeatureToken::Or),
                    _ => {
                        let (prefix, name) = split_prefixed_name(&token);
                        tokens.push(IfFeatureToken::Name(name, prefix));
                    }
                }
            }
            _ => {
                emit_error(
                    module_errors,
                    pos.clone(),
                    ErrorCode::GrammarBadArgument,
                    format!("invalid if-feature expression '{raw}'"),
                );
                return None;
            }
        }
    }

    if tokens.is_empty() {
        emit_error(
            module_errors,
            pos.clone(),
            ErrorCode::GrammarBadArgument,
            "if-feature expression cannot be empty",
        );
        return None;
    }

    Some(tokens)
}

fn collect_when_exprs(
    stmt: &Stmt,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    module_errors: &mut Vec<YError>,
) -> Vec<WhenExpr> {
    stmt.get_substmts(BuiltInKeyword::When)
        .map(|when| compile_when_expr(when, own_prefix, prefix_map, module_errors))
        .collect()
}

fn compile_when_expr(
    stmt: &Stmt,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    module_errors: &mut Vec<YError>,
) -> WhenExpr {
    let xpath = stmt.arg.clone().unwrap_or_default();
    validate_xpath(&xpath, own_prefix, prefix_map, &stmt.pos, module_errors);
    WhenExpr {
        xpath,
        description: opt_substmt_arg(stmt, BuiltInKeyword::Description),
        reference: opt_substmt_arg(stmt, BuiltInKeyword::Reference),
    }
}

fn collect_must_exprs(
    stmt: &Stmt,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    module_errors: &mut Vec<YError>,
) -> Vec<MustExpr> {
    stmt.get_substmts(BuiltInKeyword::Must)
        .map(|must| compile_must_expr(must, own_prefix, prefix_map, module_errors))
        .collect()
}

fn compile_must_expr(
    stmt: &Stmt,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    module_errors: &mut Vec<YError>,
) -> MustExpr {
    let xpath = stmt.arg.clone().unwrap_or_default();
    validate_xpath(&xpath, own_prefix, prefix_map, &stmt.pos, module_errors);
    MustExpr {
        xpath,
        error_message: opt_substmt_arg(stmt, BuiltInKeyword::ErrorMessage),
        error_app_tag: opt_substmt_arg(stmt, BuiltInKeyword::ErrorAppTag),
        description: opt_substmt_arg(stmt, BuiltInKeyword::Description),
    }
}

fn validate_xpath(
    xpath: &str,
    _own_prefix: &str,
    _prefix_map: &PrefixMap,
    pos: &Pos,
    module_errors: &mut Vec<YError>,
) {
    if xpath.trim().is_empty() {
        emit_error(
            module_errors,
            pos.clone(),
            ErrorCode::SemanticXpathSyntax,
            "XPath expression cannot be empty",
        );
    }
}

/// Expand a slice of schema nodes, lazily resolving any `SchemaNodeKind::Uses`
/// nodes into their constituent children.
pub fn expand_children(
    raw: &[SchemaNode],
    own_prefix: &str,
    module_name: &str,
    overlay: &NodeOverlayMap,
    parent_path: &[PathStep],
    ctx: &ExpansionCtx<'_>,
) -> Vec<SchemaNode> {
    let _ = (own_prefix, module_name);
    let mut result = Vec::with_capacity(raw.len());
    for node in raw {
        match &node.kind {
            SchemaNodeKind::Uses {
                grouping,
                source_module_name,
                overlay: uses_overlay,
            } => {
                let expanded = expand_uses_lazy(
                    grouping,
                    source_module_name.as_deref(),
                    uses_overlay,
                    &node.module_prefix,
                    &node.module_name,
                    overlay,
                    ctx,
                );
                result.extend(expand_children(
                    &expanded,
                    &node.module_prefix,
                    &node.module_name,
                    overlay,
                    parent_path,
                    ctx,
                ));
            }
            _ => {
                if !ctx.has_any_overlay {
                    // Fast path: no overlay anywhere — skip per-node path allocation,
                    // overlay lookup, and schema_path pmap insert.
                    let materialized = node.clone();
                    if node_visible(&materialized, ctx) {
                        result.push(materialized);
                    }
                } else {
                    let mut materialized = node.clone();
                    let node_path =
                        child_path(parent_path, &materialized.module_prefix, &materialized.name);
                    if !apply_overlay_entry(&mut materialized, overlay.get(&node_path)) {
                        continue;
                    }
                    if !node_visible(&materialized, ctx) {
                        continue;
                    }
                    attach_schema_path(&mut materialized, node_path);
                    result.push(materialized);
                }
            }
        }
    }
    result
}

fn child_path(parent_path: &[PathStep], module_prefix: &str, name: &str) -> SchemaPath {
    let mut path = parent_path.to_vec();
    path.push(PathStep {
        prefix: Some(module_prefix.to_string()),
        name: name.to_string(),
    });
    path
}

fn node_visible(node: &SchemaNode, ctx: &ExpansionCtx<'_>) -> bool {
    if let Some(max_status) = ctx.max_status {
        if node.status > max_status {
            return false;
        }
    }
    node.if_features
        .iter()
        .all(|expr| ctx.eval_if_feature(expr, &node.module_name))
}

fn apply_overlay_entry(node: &mut SchemaNode, overlay: Option<&NodeOverlay>) -> bool {
    let Some(overlay) = overlay else {
        return true;
    };
    if overlay
        .deviate_stmts
        .iter()
        .any(|stmt| stmt.arg.as_deref() == Some("not-supported"))
    {
        return false;
    }

    let mut ignored_errors = Vec::new();
    for deviate in &overlay.deviate_stmts {
        match deviate.arg.as_deref() {
            Some("add") => apply_node_mutation(
                node,
                &deviate.substmts,
                MutationMode::Add,
                &mut ignored_errors,
            ),
            Some("replace") => apply_node_mutation(
                node,
                &deviate.substmts,
                MutationMode::Replace,
                &mut ignored_errors,
            ),
            Some("delete") => apply_node_mutation(
                node,
                &deviate.substmts,
                MutationMode::Delete,
                &mut ignored_errors,
            ),
            _ => {}
        }
    }

    // Inject annotation extension instances into the node.
    for ann in &overlay.annotations {
        node.extensions.extend(ann.instances.iter().cloned());
    }

    true
}

fn fully_expand_nodes(
    raw: &[SchemaNode],
    own_prefix: &str,
    module_name: &str,
    ctx: &ExpansionCtx<'_>,
) -> Vec<SchemaNode> {
    let _ = (own_prefix, module_name);
    let empty_overlay = NodeOverlayMap::new();
    let mut result = Vec::with_capacity(raw.len());
    for node in raw {
        match &node.kind {
            SchemaNodeKind::Uses {
                grouping,
                source_module_name,
                overlay: uses_overlay,
            } => {
                let expanded = expand_uses_lazy(
                    grouping,
                    source_module_name.as_deref(),
                    uses_overlay,
                    &node.module_prefix,
                    &node.module_name,
                    &empty_overlay,
                    ctx,
                );
                result.extend(expanded.iter().cloned());
            }
            _ => {
                let mut materialized = node.clone();
                match &mut materialized.kind {
                    SchemaNodeKind::Container { children, .. }
                    | SchemaNodeKind::List { children, .. }
                    | SchemaNodeKind::Case { children }
                    | SchemaNodeKind::Notification { children, .. } => {
                        *children = fully_expand_nodes(
                            children,
                            &node.module_prefix,
                            &node.module_name,
                            ctx,
                        );
                    }
                    SchemaNodeKind::Choice { cases, .. } => {
                        *cases =
                            fully_expand_nodes(cases, &node.module_prefix, &node.module_name, ctx);
                    }
                    SchemaNodeKind::Rpc { input, output, .. }
                    | SchemaNodeKind::Action { input, output } => {
                        *input =
                            fully_expand_nodes(input, &node.module_prefix, &node.module_name, ctx);
                        *output =
                            fully_expand_nodes(output, &node.module_prefix, &node.module_name, ctx);
                    }
                    SchemaNodeKind::Leaf { .. }
                    | SchemaNodeKind::LeafList { .. }
                    | SchemaNodeKind::AnyXml { .. }
                    | SchemaNodeKind::AnyData { .. }
                    | SchemaNodeKind::Uses { .. } => {}
                }
                if node_visible(&materialized, ctx) {
                    result.push(materialized);
                }
            }
        }
    }
    result
}

fn expand_uses_lazy(
    grouping: &Arc<Grouping>,
    source_module_name: Option<&str>,
    uses_overlay: &UsesOverlay,
    own_prefix: &str,
    module_name: &str,
    overlay: &NodeOverlayMap,
    ctx: &ExpansionCtx<'_>,
) -> Arc<Vec<SchemaNode>> {
    let grouping_ptr = Arc::as_ptr(grouping);

    // Fast path: if the cache has a hit AND no mutations are needed (empty overlay and all
    // features enabled), return the Arc directly — zero Vec cloning.
    if let Some(cached_arc) = ctx.cache_get(grouping_ptr, own_prefix) {
        if uses_overlay.is_empty() && ctx.enabled_features.is_empty() {
            return cached_arc;
        }
        // Mutations needed: fall through to apply them on a cloned Vec.
        let mut nodes: Vec<SchemaNode> = (*cached_arc).clone();
        let mut ignored_errors = Vec::new();
        for refine in &uses_overlay.refine_stmts {
            apply_refine_stmt(refine, &mut nodes, &mut ignored_errors);
        }
        for augment in &uses_overlay.local_augments {
            apply_local_augment_entry(augment, &mut nodes, ctx);
        }
        if !uses_overlay.when.is_empty() || !uses_overlay.if_features.is_empty() {
            for node in &mut nodes {
                propagate_uses_constraints(node, &uses_overlay.when, &uses_overlay.if_features);
            }
        }
        let _ = overlay;
        return Arc::new(
            nodes
                .into_iter()
                .filter(|node| node_visible(node, ctx))
                .collect(),
        );
    }

    // Cache miss: compile the grouping body and populate the cache.
    let source_module = source_module_name.and_then(|name| ctx.registry.resolve_import(name, None));
    let current_module = ctx.registry.resolve_import(module_name, None);

    let pre_compiled = source_module
        .as_ref()
        .and_then(|m| m.grouping_children.get(&grouping.name))
        .or_else(|| {
            current_module
                .as_ref()
                .and_then(|m| m.grouping_children.get(&grouping.name))
        });

    let raw: Vec<SchemaNode> = if let Some(arc) = pre_compiled {
        // Fast path: clone the pre-compiled body (skips compile_schema_children).
        let mut nodes = (**arc).clone();
        if grouping.def_own_prefix != own_prefix {
            for node in &mut nodes {
                fix_module_prefix(node, module_name, own_prefix);
            }
        }
        nodes
    } else {
        // Fallback: compile from raw AST (e.g. for inline groupings inside grouping bodies
        // that weren't captured at the module level).
        let mut ignored_errors = Vec::new();
        let current_key = current_module
            .as_ref()
            .map(|m| m.key.clone())
            .unwrap_or_else(|| ModuleKey::latest(module_name));
        let empty_groupings = IndexMap::new();
        let eff_groupings_ref = source_module
            .as_ref()
            .map(|m| m.groupings.as_ref())
            .or_else(|| current_module.as_ref().map(|m| m.groupings.as_ref()))
            .unwrap_or(&empty_groupings);
        let definer_module_name = source_module_name.unwrap_or(module_name).to_string();
        let mut eff_prefix_map;
        let eff_prefix_map_ref: &PrefixMap = if !grouping
            .def_prefix_map
            .contains_key(&grouping.def_own_prefix)
        {
            eff_prefix_map = grouping.def_prefix_map.clone();
            eff_prefix_map.insert(grouping.def_own_prefix.clone(), definer_module_name);
            &eff_prefix_map
        } else {
            &grouping.def_prefix_map
        };
        let instantiated_children: Vec<Stmt> = grouping
            .stmt
            .substmts
            .iter()
            .map(|child| instantiate_stmt_for_uses(child, &grouping.stmt.pos))
            .collect();
        let mut nodes = compile_schema_children(
            &instantiated_children,
            &current_key,
            current_module
                .as_ref()
                .map(|m| m.yang_version)
                .unwrap_or(YangVersion::V11),
            &grouping.def_own_prefix,
            eff_prefix_map_ref,
            ctx.registry,
            eff_groupings_ref,
            &mut ignored_errors,
        );
        if grouping.def_own_prefix != own_prefix {
            for node in &mut nodes {
                fix_module_prefix(node, module_name, own_prefix);
            }
        }
        nodes
    };

    // Flatten nested `uses` nodes before caching.
    let expanded = fully_expand_nodes(&raw, own_prefix, module_name, ctx);
    let cached_arc = Arc::new(expanded);
    ctx.cache_insert(grouping_ptr, own_prefix, Arc::clone(&cached_arc));

    // Now apply the UsesOverlay on top of the cached (immutable) base.
    if uses_overlay.is_empty() && ctx.enabled_features.is_empty() {
        return cached_arc;
    }
    let mut nodes: Vec<SchemaNode> = (*cached_arc).clone();
    let mut ignored_errors = Vec::new();
    for refine in &uses_overlay.refine_stmts {
        apply_refine_stmt(refine, &mut nodes, &mut ignored_errors);
    }
    for augment in &uses_overlay.local_augments {
        apply_local_augment_entry(augment, &mut nodes, ctx);
    }
    if !uses_overlay.when.is_empty() || !uses_overlay.if_features.is_empty() {
        for node in &mut nodes {
            propagate_uses_constraints(node, &uses_overlay.when, &uses_overlay.if_features);
        }
    }
    let _ = overlay;
    Arc::new(
        nodes
            .into_iter()
            .filter(|node| node_visible(node, ctx))
            .collect(),
    )
}

fn apply_local_augment_entry(
    augment: &LocalAugmentEntry,
    nodes: &mut Vec<SchemaNode>,
    ctx: &ExpansionCtx<'_>,
) {
    let mut additions = fully_expand_nodes(&augment.nodes, "", "", ctx);
    if !augment.when.is_empty() || !augment.if_features.is_empty() {
        for node in &mut additions {
            propagate_uses_constraints(node, &augment.when, &augment.if_features);
        }
    }
    inline_local_augment_into(&augment.target_path, additions, nodes);
}

fn inline_local_augment_into(
    target_path: &[String],
    nodes: Vec<SchemaNode>,
    children: &mut Vec<SchemaNode>,
) {
    if target_path.is_empty() {
        children.extend(nodes);
        return;
    }

    let path: Vec<PathStep> = target_path
        .iter()
        .map(|name| PathStep {
            prefix: None,
            name: name.clone(),
        })
        .collect();
    if let Some(target) = find_node_mut(children, &path) {
        match &mut target.kind {
            SchemaNodeKind::Container { children: c, .. }
            | SchemaNodeKind::List { children: c, .. }
            | SchemaNodeKind::Case { children: c }
            | SchemaNodeKind::Notification { children: c, .. } => c.extend(nodes),
            SchemaNodeKind::Choice { cases, .. } => cases.extend(nodes),
            _ => {}
        }
    }
}

/// Recursively update `module_prefix` on all nodes that belong to `module_name`.
/// Used after grouping expansion to ensure nodes carry the using module's prefix
/// rather than the grouping definer's prefix (for tree-diagram display).
fn fix_module_prefix(node: &mut SchemaNode, module_name: &str, new_prefix: &str) {
    if node.module_name == module_name {
        node.module_prefix = new_prefix.to_string();
    }
    match &mut node.kind {
        SchemaNodeKind::Container { children, .. }
        | SchemaNodeKind::List { children, .. }
        | SchemaNodeKind::Notification { children, .. }
        | SchemaNodeKind::Case { children } => {
            for child in children {
                fix_module_prefix(child, module_name, new_prefix);
            }
        }
        SchemaNodeKind::Choice { cases, .. } => {
            for case in cases {
                fix_module_prefix(case, module_name, new_prefix);
            }
        }
        SchemaNodeKind::Rpc { input, output, .. } | SchemaNodeKind::Action { input, output } => {
            for child in input.iter_mut().chain(output.iter_mut()) {
                fix_module_prefix(child, module_name, new_prefix);
            }
        }
        _ => {}
    }
}

fn resolve_grouping(
    prefix: Option<&str>,
    name: &str,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    registry: &ModuleRegistry,
    local_groupings: &IndexMap<String, Grouping>,
    pos: &Pos,
    module_errors: &mut Vec<YError>,
) -> Option<Grouping> {
    if name.is_empty() {
        emit_error(
            module_errors,
            pos.clone(),
            ErrorCode::GrammarMissingRequired,
            "uses requires a grouping name",
        );
        return None;
    }

    // A module may use its own prefix to reference local groupings (RFC 7950 §7.13).
    // Treat self-prefix as unprefixed so it falls through to the local lookup below.
    let prefix = prefix.filter(|p| *p != own_prefix);

    if let Some(prefix) = prefix {
        let Some(module_name) = prefix_map.get(prefix) else {
            emit_error(
                module_errors,
                pos.clone(),
                ErrorCode::SemanticUnknownPrefix,
                format!("unknown prefix '{prefix}' in uses statement"),
            );
            return None;
        };

        let Some(module) = registry.resolve_import(module_name, None) else {
            emit_error(
                module_errors,
                pos.clone(),
                ErrorCode::ModuleNotFound,
                format!("module '{module_name}' for prefix '{prefix}' not found"),
            );
            return None;
        };

        let Some(grouping) = module.groupings.get(name) else {
            emit_error(
                module_errors,
                pos.clone(),
                ErrorCode::SemanticUnknownGrouping,
                format!("unknown grouping '{prefix}:{name}'"),
            );
            return None;
        };

        Some(grouping.clone())
    } else {
        let Some(grouping) = local_groupings.get(name) else {
            emit_error(
                module_errors,
                pos.clone(),
                ErrorCode::SemanticUnknownGrouping,
                format!("unknown grouping '{name}'"),
            );
            return None;
        };
        Some(grouping.clone())
    }
}

fn instantiate_stmt_for_uses(stmt: &Stmt, uses_pos: &Pos) -> Stmt {
    Stmt {
        keyword: stmt.keyword.clone(),
        arg: stmt.arg.clone(),
        pos: Pos::UsesPos {
            uses_pos: Box::new(uses_pos.clone()),
            orig_pos: Box::new(stmt.pos.clone()),
        },
        substmts: stmt
            .substmts
            .iter()
            .map(|sub| instantiate_stmt_for_uses(sub, uses_pos))
            .collect(),
    }
}

fn apply_refine_stmt(refine: &Stmt, nodes: &mut Vec<SchemaNode>, module_errors: &mut Vec<YError>) {
    let Some(path) = parse_relative_schema_path(
        refine.arg.as_deref().unwrap_or(""),
        &refine.pos,
        module_errors,
    ) else {
        return;
    };

    let Some(target) = find_node_mut(nodes, &path) else {
        emit_error(
            module_errors,
            refine.pos.clone(),
            ErrorCode::SemanticInvalidAugmentTarget,
            format!(
                "refine target '{}' not found",
                refine.arg.as_deref().unwrap_or("")
            ),
        );
        return;
    };

    apply_node_mutation(
        target,
        &refine.substmts,
        MutationMode::Replace,
        module_errors,
    );
}

fn propagate_uses_constraints(
    node: &mut SchemaNode,
    inherited_when: &[WhenExpr],
    inherited_if_features: &[IfFeatureExpr],
) {
    if !inherited_when.is_empty() {
        let mut combined = inherited_when.to_vec();
        combined.extend(node.when.clone());
        node.when = combined;
    }
    if !inherited_if_features.is_empty() {
        let mut combined = inherited_if_features.to_vec();
        combined.extend(node.if_features.clone());
        node.if_features = combined;
    }

    match &mut node.kind {
        SchemaNodeKind::Container { children, .. }
        | SchemaNodeKind::List { children, .. }
        | SchemaNodeKind::Case { children }
        | SchemaNodeKind::Notification { children, .. } => {
            for child in children {
                propagate_uses_constraints(child, inherited_when, inherited_if_features);
            }
        }
        SchemaNodeKind::Choice { cases, .. } => {
            for case in cases {
                propagate_uses_constraints(case, inherited_when, inherited_if_features);
            }
        }
        SchemaNodeKind::Rpc { input, output, .. } | SchemaNodeKind::Action { input, output } => {
            for child in input {
                propagate_uses_constraints(child, inherited_when, inherited_if_features);
            }
            for child in output {
                propagate_uses_constraints(child, inherited_when, inherited_if_features);
            }
        }
        SchemaNodeKind::Leaf { .. }
        | SchemaNodeKind::LeafList { .. }
        | SchemaNodeKind::AnyXml { .. }
        | SchemaNodeKind::AnyData { .. }
        | SchemaNodeKind::Uses { .. } => {}
    }
}

fn apply_deviations(
    key: &ModuleKey,
    children: &mut Vec<SchemaNode>,
    _registry: &ModuleRegistry,
    dev_index: &DeviationIndex,
    module_errors: &mut Vec<YError>,
    overlay: &mut NodeOverlayMap,
) {
    for deviations in dev_index.by_deviating_module.values() {
        for deviation in deviations {
            if !deviation_targets_module(deviation, key) {
                continue;
            }

            let Some(target_path) =
                parse_path_internal(&deviation.target_path, true, &deviation.pos, module_errors)
            else {
                continue;
            };

            if !deviation_leaf_targets_module(&target_path, key, deviation) {
                continue;
            }

            let target_found = find_node_mut(children, &target_path).is_some();
            if !target_found {
                overlay
                    .entry(target_path.clone())
                    .or_default()
                    .deviate_stmts
                    .extend(deviation.deviate_stmts.iter().cloned());
                continue;
            }

            for deviate in &deviation.deviate_stmts {
                match deviate.arg.as_deref() {
                    Some("not-supported") => {
                        if !remove_node_at_path(children, &target_path) {
                            overlay
                                .entry(target_path.clone())
                                .or_default()
                                .deviate_stmts
                                .push(deviate.clone());
                        }
                    }
                    Some("add") => apply_deviation_edit(
                        children,
                        &target_path,
                        &deviate.substmts,
                        MutationMode::Add,
                        &deviation.pos,
                        module_errors,
                    ),
                    Some("replace") => apply_deviation_edit(
                        children,
                        &target_path,
                        &deviate.substmts,
                        MutationMode::Replace,
                        &deviation.pos,
                        module_errors,
                    ),
                    Some("delete") => apply_deviation_edit(
                        children,
                        &target_path,
                        &deviate.substmts,
                        MutationMode::Delete,
                        &deviation.pos,
                        module_errors,
                    ),
                    Some(other) => emit_error(
                        module_errors,
                        deviate.pos.clone(),
                        ErrorCode::GrammarBadArgument,
                        format!("unsupported deviate argument '{other}'"),
                    ),
                    None => emit_error(
                        module_errors,
                        deviate.pos.clone(),
                        ErrorCode::GrammarMissingRequired,
                        "deviate requires an argument",
                    ),
                }
            }
        }
    }
}

fn apply_annotations(
    key: &ModuleKey,
    children: &mut Vec<SchemaNode>,
    ann_index: &AnnotationIndex,
    module_errors: &mut Vec<YError>,
    overlay: &mut NodeOverlayMap,
) {
    let Some(pending) = ann_index.by_target_module.get(&key.name) else {
        return;
    };

    for ann in pending {
        if !annotation_targets_module(ann, key) {
            continue;
        }

        let Some(target_path) =
            parse_path_internal(&ann.target_path, true, &ann.pos, module_errors)
        else {
            continue;
        };

        if !annotation_leaf_targets_module(&target_path, key, ann) {
            continue;
        }

        if find_node_mut(children, &target_path).is_some() {
            let node = find_node_mut(children, &target_path).unwrap();
            node.extensions.extend(ann.instances.iter().cloned());
        } else {
            // Target is inside an unexpanded `uses` — defer to overlay.
            overlay
                .entry(target_path)
                .or_default()
                .annotations
                .push(crate::compiler::types::Annotation {
                    instances: ann.instances.clone(),
                    source_plugin: ann.source_plugin,
                });
        }
    }
}

fn annotation_targets_module(ann: &PendingAnnotation, key: &ModuleKey) -> bool {
    match &ann.target_module_name {
        Some(module_name) => module_name == &key.name,
        None => match &ann.target_prefix {
            None => ann.from_module.name == key.name,
            Some(_) => false,
        },
    }
}

fn annotation_leaf_targets_module(
    path: &[PathStep],
    key: &ModuleKey,
    ann: &PendingAnnotation,
) -> bool {
    let Some(last) = path.last() else {
        return true;
    };
    let Some(ref prefix) = last.prefix else {
        return true;
    };
    match ann.prefix_map.get(prefix) {
        Some(module_name) => module_name == &key.name,
        None => false,
    }
}

/// Returns `true` if `deviation` should be applied when compiling module `key`.
///
/// The target module name was resolved from the deviating module's imports at
/// devindex build time, so no registry lookup or prefix-string comparison is
/// needed.  Using the pre-resolved name is also correct when many modules share
/// the same self-declared prefix letter (e.g. all Cisco-IOS-XE submodules use
/// `prefix ios`).
fn deviation_targets_module(deviation: &PendingDeviation, key: &ModuleKey) -> bool {
    match &deviation.target_module_name {
        // Prefix resolved to a known module: only apply to that module.
        Some(module_name) => module_name == &key.name,
        // No target module name: either the path had no prefix (self-deviation)
        // or the prefix wasn't found in the imports (malformed file).
        None => match &deviation.target_prefix {
            // No prefix on the path → targets the deviating module itself.
            None => deviation.from_module.name == key.name,
            // Prefix present but unresolved → cannot determine target; skip.
            Some(_) => false,
        },
    }
}

/// Returns `false` if the most-specific (last) element of `path` carries a
/// prefix that resolves to a module other than `key`.  Such a deviation targets
/// a node contributed by a foreign augment, not by the module being compiled,
/// so it should be silently skipped.  When the prefix cannot be resolved we
/// conservatively return `true` to keep the existing error.
fn deviation_leaf_targets_module(
    path: &[PathStep],
    key: &ModuleKey,
    deviation: &PendingDeviation,
) -> bool {
    let Some(last) = path.last() else {
        return true;
    };
    let Some(ref prefix) = last.prefix else {
        // No prefix on the leaf step → inherits the root namespace which is
        // already confirmed to be this module.
        return true;
    };
    // Resolve the prefix using the deviating module's import map, which was
    // captured at devindex build time from the AST — no registry lookup needed.
    match deviation.prefix_map.get(prefix) {
        Some(module_name) => module_name == &key.name,
        // Prefix not in any import → refers to something entirely outside
        // the current context; skip silently.
        None => false,
    }
}

fn apply_deviation_edit(
    nodes: &mut Vec<SchemaNode>,
    target_path: &[PathStep],
    stmts: &[Stmt],
    mode: MutationMode,
    pos: &Pos,
    module_errors: &mut Vec<YError>,
) {
    let Some(target) = find_node_mut(nodes, target_path) else {
        emit_error(
            module_errors,
            pos.clone(),
            ErrorCode::SemanticInvalidDeviationTarget,
            "deviation target not found",
        );
        return;
    };

    apply_node_mutation(target, stmts, mode, module_errors);
}

fn apply_node_mutation(
    node: &mut SchemaNode,
    stmts: &[Stmt],
    mode: MutationMode,
    module_errors: &mut Vec<YError>,
) {
    for stmt in stmts {
        match builtin_keyword(stmt) {
            Some(BuiltInKeyword::Config) => match mode {
                MutationMode::Delete => node.config = None,
                MutationMode::Add if node.config.is_some() => {}
                _ => node.config = opt_bool_arg(stmt, module_errors),
            },
            Some(BuiltInKeyword::Status) => match mode {
                MutationMode::Delete => node.status = Status::Current,
                MutationMode::Add if node.status != Status::Current => {}
                _ => node.status = parse_status_arg(stmt, module_errors),
            },
            Some(BuiltInKeyword::Description) => {
                apply_opt_string(mode, &mut node.description, stmt.arg.clone())
            }
            Some(BuiltInKeyword::Reference) => {
                apply_opt_string(mode, &mut node.reference, stmt.arg.clone())
            }
            Some(BuiltInKeyword::When) => mutate_when_exprs(&mut node.when, stmt, mode),
            Some(BuiltInKeyword::IfFeature) => {
                if mode == MutationMode::Delete {
                    node.if_features.clear();
                }
            }
            Some(BuiltInKeyword::Must) => match &mut node.kind {
                SchemaNodeKind::Container { musts, .. }
                | SchemaNodeKind::Leaf { musts, .. }
                | SchemaNodeKind::LeafList { musts, .. }
                | SchemaNodeKind::List { musts, .. }
                | SchemaNodeKind::Rpc { musts, .. }
                | SchemaNodeKind::Notification { musts, .. }
                | SchemaNodeKind::AnyXml { musts, .. }
                | SchemaNodeKind::AnyData { musts, .. } => mutate_must_exprs(musts, stmt, mode),
                _ => {}
            },
            Some(BuiltInKeyword::Mandatory) => match &mut node.kind {
                SchemaNodeKind::Leaf { mandatory, .. }
                | SchemaNodeKind::Choice { mandatory, .. }
                | SchemaNodeKind::AnyXml { mandatory, .. }
                | SchemaNodeKind::AnyData { mandatory, .. } => {
                    if mode == MutationMode::Delete {
                        *mandatory = false;
                    } else if mode != MutationMode::Add || !*mandatory {
                        *mandatory = opt_bool_arg(stmt, module_errors).unwrap_or(false);
                    }
                }
                _ => {}
            },
            Some(BuiltInKeyword::Presence) => {
                if let SchemaNodeKind::Container { presence, .. } = &mut node.kind {
                    apply_opt_string(mode, presence, stmt.arg.clone());
                }
            }
            Some(BuiltInKeyword::Type) => match &mut node.kind {
                SchemaNodeKind::Leaf { type_stmt, .. }
                | SchemaNodeKind::LeafList { type_stmt, .. } => {
                    if mode != MutationMode::Delete {
                        *type_stmt = stmt.clone();
                    }
                }
                _ => {}
            },
            Some(BuiltInKeyword::Units) => match &mut node.kind {
                SchemaNodeKind::Leaf { units, .. } | SchemaNodeKind::LeafList { units, .. } => {
                    apply_opt_string(mode, units, stmt.arg.clone())
                }
                _ => {}
            },
            Some(BuiltInKeyword::Default) => match &mut node.kind {
                SchemaNodeKind::Leaf { default, .. } => {
                    apply_opt_string(mode, default, stmt.arg.clone())
                }
                SchemaNodeKind::LeafList { default, .. } => mutate_string_list(default, stmt, mode),
                SchemaNodeKind::Choice { default, .. } => {
                    apply_opt_string(mode, default, stmt.arg.clone())
                }
                _ => {}
            },
            Some(BuiltInKeyword::MinElements) => match &mut node.kind {
                SchemaNodeKind::LeafList { min_elements, .. }
                | SchemaNodeKind::List { min_elements, .. } => {
                    if mode == MutationMode::Delete {
                        *min_elements = 0;
                    } else {
                        *min_elements = parse_u64_arg(stmt, module_errors).unwrap_or(*min_elements);
                    }
                }
                _ => {}
            },
            Some(BuiltInKeyword::MaxElements) => match &mut node.kind {
                SchemaNodeKind::LeafList { max_elements, .. }
                | SchemaNodeKind::List { max_elements, .. } => {
                    if mode == MutationMode::Delete {
                        *max_elements = None;
                    } else {
                        *max_elements = parse_max_arg(stmt, module_errors);
                    }
                }
                _ => {}
            },
            Some(BuiltInKeyword::OrderedBy) => match &mut node.kind {
                SchemaNodeKind::LeafList { ordered_by, .. }
                | SchemaNodeKind::List { ordered_by, .. } => {
                    if mode == MutationMode::Delete {
                        *ordered_by = OrderedBy::System;
                    } else {
                        *ordered_by =
                            parse_ordered_by_arg(stmt, module_errors).unwrap_or(*ordered_by);
                    }
                }
                _ => {}
            },
            Some(BuiltInKeyword::Unique) => {
                if let SchemaNodeKind::List { unique, .. } = &mut node.kind {
                    mutate_string_list(unique, stmt, mode);
                }
            }
            Some(BuiltInKeyword::Key) => {
                if let SchemaNodeKind::List { key, .. } = &mut node.kind {
                    if mode == MutationMode::Delete {
                        key.clear();
                    } else {
                        *key = parse_key_stmt(Some(stmt));
                    }
                }
            }
            _ => {}
        }
    }
}

fn mutate_when_exprs(list: &mut Vec<WhenExpr>, stmt: &Stmt, mode: MutationMode) {
    let entry = WhenExpr {
        xpath: stmt.arg.clone().unwrap_or_default(),
        description: opt_substmt_arg(stmt, BuiltInKeyword::Description),
        reference: opt_substmt_arg(stmt, BuiltInKeyword::Reference),
    };
    match mode {
        MutationMode::Add => list.push(entry),
        MutationMode::Replace => {
            list.clear();
            list.push(entry);
        }
        MutationMode::Delete => list.retain(|when| when.xpath != entry.xpath),
    }
}

fn mutate_must_exprs(list: &mut Vec<MustExpr>, stmt: &Stmt, mode: MutationMode) {
    let entry = MustExpr {
        xpath: stmt.arg.clone().unwrap_or_default(),
        error_message: opt_substmt_arg(stmt, BuiltInKeyword::ErrorMessage),
        error_app_tag: opt_substmt_arg(stmt, BuiltInKeyword::ErrorAppTag),
        description: opt_substmt_arg(stmt, BuiltInKeyword::Description),
    };
    match mode {
        MutationMode::Add => list.push(entry),
        MutationMode::Replace => {
            list.clear();
            list.push(entry);
        }
        MutationMode::Delete => list.retain(|must| must.xpath != entry.xpath),
    }
}

fn mutate_string_list(list: &mut Vec<String>, stmt: &Stmt, mode: MutationMode) {
    let value = stmt.arg.clone().unwrap_or_default();
    match mode {
        MutationMode::Add => list.push(value),
        MutationMode::Replace => {
            list.clear();
            list.push(value);
        }
        MutationMode::Delete => list.retain(|existing| existing != &value),
    }
}

fn apply_opt_string(mode: MutationMode, slot: &mut Option<String>, value: Option<String>) {
    match mode {
        MutationMode::Delete => *slot = None,
        MutationMode::Add if slot.is_some() => {}
        _ => *slot = value,
    }
}

fn find_node_mut<'a>(
    nodes: &'a mut Vec<SchemaNode>,
    path: &[PathStep],
) -> Option<&'a mut SchemaNode> {
    let (head, tail) = path.split_first()?;
    for node in nodes.iter_mut() {
        if node.name != head.name {
            continue;
        }
        if tail.is_empty() {
            return Some(node);
        }
        return find_node_mut_in_node(node, tail);
    }
    None
}

fn find_node_mut_in_node<'a>(
    node: &'a mut SchemaNode,
    path: &[PathStep],
) -> Option<&'a mut SchemaNode> {
    let (head, tail) = path.split_first()?;
    match &mut node.kind {
        SchemaNodeKind::Container { children, .. }
        | SchemaNodeKind::List { children, .. }
        | SchemaNodeKind::Case { children }
        | SchemaNodeKind::Notification { children, .. } => find_node_mut(children, path),
        SchemaNodeKind::Choice { cases, .. } => find_node_mut(cases, path),
        SchemaNodeKind::Rpc { input, output, .. } | SchemaNodeKind::Action { input, output } => {
            if head.name == "input" {
                if tail.is_empty() {
                    None
                } else {
                    find_node_mut(input, tail)
                }
            } else if head.name == "output" {
                if tail.is_empty() {
                    None
                } else {
                    find_node_mut(output, tail)
                }
            } else {
                None
            }
        }
        SchemaNodeKind::Leaf { .. }
        | SchemaNodeKind::LeafList { .. }
        | SchemaNodeKind::AnyXml { .. }
        | SchemaNodeKind::AnyData { .. }
        | SchemaNodeKind::Uses { .. } => None,
    }
}

fn remove_node_at_path(nodes: &mut Vec<SchemaNode>, path: &[PathStep]) -> bool {
    let Some((head, tail)) = path.split_first() else {
        return false;
    };

    if tail.is_empty() {
        if let Some(idx) = nodes.iter().position(|node| node.name == head.name) {
            nodes.remove(idx);
            return true;
        }
        return false;
    }

    if let Some(node) = nodes.iter_mut().find(|node| node.name == head.name) {
        match &mut node.kind {
            SchemaNodeKind::Container { children, .. }
            | SchemaNodeKind::List { children, .. }
            | SchemaNodeKind::Case { children }
            | SchemaNodeKind::Notification { children, .. } => remove_node_at_path(children, tail),
            SchemaNodeKind::Choice { cases, .. } => remove_node_at_path(cases, tail),
            SchemaNodeKind::Rpc { input, output, .. }
            | SchemaNodeKind::Action { input, output } => {
                if tail[0].name == "input" {
                    if tail.len() > 1 {
                        remove_node_at_path(input, &tail[1..])
                    } else {
                        false
                    }
                } else if tail[0].name == "output" {
                    if tail.len() > 1 {
                        remove_node_at_path(output, &tail[1..])
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            _ => false,
        }
    } else {
        false
    }
}

fn parse_absolute_schema_path(
    raw: &str,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    pos: &Pos,
    module_errors: &mut Vec<YError>,
) -> Option<SchemaPath> {
    let path = parse_path_internal(raw, true, pos, module_errors)?;
    validate_path_prefixes(&path, own_prefix, prefix_map, pos, module_errors);
    Some(path)
}

fn parse_relative_schema_path(
    raw: &str,
    pos: &Pos,
    module_errors: &mut Vec<YError>,
) -> Option<SchemaPath> {
    parse_path_internal(raw, false, pos, module_errors)
}

fn parse_path_internal(
    raw: &str,
    absolute: bool,
    pos: &Pos,
    module_errors: &mut Vec<YError>,
) -> Option<SchemaPath> {
    if absolute && !raw.starts_with('/') {
        emit_error(
            module_errors,
            pos.clone(),
            ErrorCode::GrammarBadArgument,
            format!("expected absolute schema path, got '{raw}'"),
        );
        return None;
    }
    if !absolute && raw.starts_with('/') {
        emit_error(
            module_errors,
            pos.clone(),
            ErrorCode::GrammarBadArgument,
            format!("expected relative schema path, got '{raw}'"),
        );
        return None;
    }

    let trimmed = if absolute {
        raw.trim_start_matches('/')
    } else {
        raw
    };
    if trimmed.is_empty() {
        emit_error(
            module_errors,
            pos.clone(),
            ErrorCode::GrammarBadArgument,
            "schema path cannot be empty",
        );
        return None;
    }

    let mut path = Vec::new();
    for step in trimmed.split('/') {
        if step.is_empty() {
            continue;
        }
        let (prefix, name) = split_prefixed_name(step);
        if name.is_empty() {
            emit_error(
                module_errors,
                pos.clone(),
                ErrorCode::GrammarBadArgument,
                format!("invalid schema path step '{step}'"),
            );
            return None;
        }
        path.push(PathStep { prefix, name });
    }

    if path.is_empty() {
        emit_error(
            module_errors,
            pos.clone(),
            ErrorCode::GrammarBadArgument,
            "schema path cannot be empty",
        );
        return None;
    }

    Some(path)
}

fn validate_path_prefixes(
    path: &[PathStep],
    own_prefix: &str,
    prefix_map: &PrefixMap,
    pos: &Pos,
    module_errors: &mut Vec<YError>,
) {
    for step in path {
        if let Some(prefix) = &step.prefix {
            validate_known_prefix(prefix, own_prefix, prefix_map, pos, module_errors);
        }
    }
}

fn validate_known_prefix(
    prefix: &str,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    pos: &Pos,
    module_errors: &mut Vec<YError>,
) {
    if prefix != own_prefix && !prefix_map.contains_key(prefix) {
        emit_error(
            module_errors,
            pos.clone(),
            ErrorCode::SemanticUnknownPrefix,
            format!("unknown prefix '{prefix}'"),
        );
    }
}

fn builtin_keyword(stmt: &Stmt) -> Option<BuiltInKeyword> {
    match stmt.keyword {
        Keyword::BuiltIn(keyword) => Some(keyword),
        _ => None,
    }
}

fn parse_key_stmt(stmt: Option<&Stmt>) -> Vec<String> {
    stmt.and_then(|stmt| stmt.arg.clone())
        .map(|arg| arg.split_whitespace().map(ToOwned::to_owned).collect())
        .unwrap_or_default()
}

fn parse_status(stmt: &Stmt, module_errors: &mut Vec<YError>) -> Status {
    stmt.get_substmt(BuiltInKeyword::Status)
        .map(|status| parse_status_arg(status, module_errors))
        .unwrap_or(Status::Current)
}

fn parse_status_arg(stmt: &Stmt, module_errors: &mut Vec<YError>) -> Status {
    match stmt.arg.as_deref() {
        Some("current") => Status::Current,
        Some("deprecated") => Status::Deprecated,
        Some("obsolete") => Status::Obsolete,
        Some(other) => {
            emit_error(
                module_errors,
                stmt.pos.clone(),
                ErrorCode::GrammarBadArgument,
                format!("invalid status '{other}'"),
            );
            Status::Current
        }
        None => {
            emit_error(
                module_errors,
                stmt.pos.clone(),
                ErrorCode::GrammarMissingRequired,
                "status requires an argument",
            );
            Status::Current
        }
    }
}

fn opt_ordered_by(stmt: &Stmt, module_errors: &mut Vec<YError>) -> Option<OrderedBy> {
    stmt.get_substmt(BuiltInKeyword::OrderedBy)
        .and_then(|ordered_by| parse_ordered_by_arg(ordered_by, module_errors))
}

fn parse_ordered_by_arg(stmt: &Stmt, module_errors: &mut Vec<YError>) -> Option<OrderedBy> {
    match stmt.arg.as_deref() {
        Some("system") => Some(OrderedBy::System),
        Some("user") => Some(OrderedBy::User),
        Some(other) => {
            emit_error(
                module_errors,
                stmt.pos.clone(),
                ErrorCode::GrammarBadArgument,
                format!("invalid ordered-by value '{other}'"),
            );
            None
        }
        None => {
            emit_error(
                module_errors,
                stmt.pos.clone(),
                ErrorCode::GrammarMissingRequired,
                "ordered-by requires an argument",
            );
            None
        }
    }
}

fn opt_bool_substmt(
    stmt: &Stmt,
    keyword: BuiltInKeyword,
    module_errors: &mut Vec<YError>,
) -> Option<bool> {
    stmt.get_substmt(keyword)
        .and_then(|substmt| opt_bool_arg(substmt, module_errors))
}

fn opt_bool_arg(stmt: &Stmt, module_errors: &mut Vec<YError>) -> Option<bool> {
    match stmt.arg.as_deref() {
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some(other) => {
            emit_error(
                module_errors,
                stmt.pos.clone(),
                ErrorCode::GrammarBadArgument,
                format!("invalid boolean value '{other}'"),
            );
            None
        }
        None => {
            emit_error(
                module_errors,
                stmt.pos.clone(),
                ErrorCode::GrammarMissingRequired,
                format!("{} requires an argument", stmt.keyword),
            );
            None
        }
    }
}

fn opt_u64_substmt(
    stmt: &Stmt,
    keyword: BuiltInKeyword,
    module_errors: &mut Vec<YError>,
) -> Option<u64> {
    stmt.get_substmt(keyword)
        .and_then(|substmt| parse_u64_arg(substmt, module_errors))
}

fn parse_u64_arg(stmt: &Stmt, module_errors: &mut Vec<YError>) -> Option<u64> {
    match stmt.arg.as_deref() {
        Some(raw) => match raw.parse::<u64>() {
            Ok(value) => Some(value),
            Err(_) => {
                emit_error(
                    module_errors,
                    stmt.pos.clone(),
                    ErrorCode::GrammarBadArgument,
                    format!("invalid non-negative integer '{raw}'"),
                );
                None
            }
        },
        None => {
            emit_error(
                module_errors,
                stmt.pos.clone(),
                ErrorCode::GrammarMissingRequired,
                format!("{} requires an argument", stmt.keyword),
            );
            None
        }
    }
}

fn opt_max_elements(stmt: &Stmt, module_errors: &mut Vec<YError>) -> Option<u64> {
    stmt.get_substmt(BuiltInKeyword::MaxElements)
        .and_then(|substmt| parse_max_arg(substmt, module_errors))
}

fn parse_max_arg(stmt: &Stmt, module_errors: &mut Vec<YError>) -> Option<u64> {
    match stmt.arg.as_deref() {
        Some("unbounded") => None,
        Some(raw) => match raw.parse::<u64>() {
            Ok(value) => Some(value),
            Err(_) => {
                emit_error(
                    module_errors,
                    stmt.pos.clone(),
                    ErrorCode::GrammarBadArgument,
                    format!("invalid max-elements value '{raw}'"),
                );
                None
            }
        },
        None => {
            emit_error(
                module_errors,
                stmt.pos.clone(),
                ErrorCode::GrammarMissingRequired,
                "max-elements requires an argument",
            );
            None
        }
    }
}

fn required_stmt_name(stmt: &Stmt, module_errors: &mut Vec<YError>) -> Option<String> {
    let Some(arg) = stmt.arg.clone() else {
        emit_error(
            module_errors,
            stmt.pos.clone(),
            ErrorCode::GrammarMissingRequired,
            format!("{} requires an argument", stmt.keyword),
        );
        return None;
    };

    if arg.is_empty() {
        emit_error(
            module_errors,
            stmt.pos.clone(),
            ErrorCode::GrammarMissingRequired,
            format!("{} requires a non-empty argument", stmt.keyword),
        );
        return None;
    }

    Some(arg)
}

fn required_substmt_arg(
    stmt: &Stmt,
    keyword: BuiltInKeyword,
    module_errors: &mut Vec<YError>,
    missing_message: &str,
) -> Option<String> {
    let Some(substmt) = stmt.get_substmt(keyword) else {
        emit_error(
            module_errors,
            stmt.pos.clone(),
            ErrorCode::GrammarMissingRequired,
            missing_message,
        );
        return None;
    };

    if let Some(arg) = &substmt.arg {
        Some(arg.clone())
    } else {
        emit_error(
            module_errors,
            substmt.pos.clone(),
            ErrorCode::GrammarMissingRequired,
            format!("{} requires an argument", keyword),
        );
        None
    }
}

fn opt_substmt_arg(stmt: &Stmt, keyword: BuiltInKeyword) -> Option<String> {
    stmt.get_substmt(keyword)
        .and_then(|substmt| substmt.arg.clone())
}

fn split_prefixed_name(raw: &str) -> (Option<String>, String) {
    if let Some((prefix, name)) = raw.split_once(':') {
        (Some(prefix.to_string()), name.to_string())
    } else {
        (None, raw.to_string())
    }
}

fn missing_type_stmt(stmt: &Stmt, module_errors: &mut Vec<YError>) -> Stmt {
    emit_error(
        module_errors,
        stmt.pos.clone(),
        ErrorCode::GrammarMissingRequired,
        format!("{} missing required type statement", stmt.keyword),
    );
    Stmt::new(
        Keyword::BuiltIn(BuiltInKeyword::Type),
        Some("string".to_string()),
        stmt.pos.clone(),
        vec![],
    )
}

fn emit_error(
    module_errors: &mut Vec<YError>,
    pos: Pos,
    code: ErrorCode,
    message: impl Into<String>,
) {
    module_errors.push(YError {
        level: Level::Error,
        pos,
        code,
        message: message.into(),
    });
}

fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_yang;
    use std::sync::Arc;

    fn parse_module(source: &str) -> Stmt {
        let (stmts, errors) = parse_yang(source, Arc::from("test.yang"));
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        stmts.into_iter().next().expect("module stmt")
    }

    #[test]
    fn compiles_module_header() {
        let stmt = parse_module(
            r#"
module example {
  yang-version 1.1;
  namespace "urn:example";
  prefix ex;
}
"#,
        );

        let compiled = compile_module(
            &ModuleKey::latest("example"),
            stmt,
            &ModuleRegistry::default(),
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
        );

        assert!(
            compiled.errors.is_empty(),
            "compile errors: {:?}",
            compiled.errors
        );
        assert_eq!(compiled.yang_version, YangVersion::V11);
        assert_eq!(compiled.namespace, "urn:example");
        assert_eq!(compiled.prefix, "ex");
    }

    #[test]
    fn builds_prefix_map_from_imports() {
        let dep_stmt = parse_module(
            r#"
module dep {
  namespace "urn:dep";
  prefix dep;
}
"#,
        );
        let dep_compiled = std::sync::Arc::new(compile_module(
            &ModuleKey::latest("dep"),
            dep_stmt,
            &ModuleRegistry::default(),
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
        ));
        assert!(
            dep_compiled.errors.is_empty(),
            "seed compile errors: {:?}",
            dep_compiled.errors
        );

        let mut registry = ModuleRegistry::default();
        registry.insert(dep_compiled);

        let stmt = parse_module(
            r#"
module main {
  namespace "urn:main";
  prefix main;
  import dep {
    prefix d;
  }
}
"#,
        );

        let compiled = compile_module(
            &ModuleKey::latest("main"),
            stmt,
            &registry,
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
        );

        assert!(
            compiled.errors.is_empty(),
            "compile errors: {:?}",
            compiled.errors
        );
        assert_eq!(compiled.prefix_map.get("d"), Some(&"dep".to_string()));
    }

    #[test]
    fn compiles_leaf_node() {
        let stmt = parse_module(
            r#"
module leafmod {
  namespace "urn:leafmod";
  prefix lf;
  leaf hostname {
    type string;
    units "chars";
    mandatory true;
  }
}
"#,
        );

        let compiled = compile_module(
            &ModuleKey::latest("leafmod"),
            stmt,
            &ModuleRegistry::default(),
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
        );

        assert!(
            compiled.errors.is_empty(),
            "compile errors: {:?}",
            compiled.errors
        );
        assert_eq!(compiled.children.len(), 1);
        let leaf = &compiled.children[0];
        assert_eq!(leaf.name, "hostname");
        match &leaf.kind {
            SchemaNodeKind::Leaf {
                type_stmt,
                units,
                mandatory,
                default,
                ..
            } => {
                assert_eq!(type_stmt.arg.as_deref(), Some("string"));
                assert_eq!(units.as_deref(), Some("chars"));
                assert!(*mandatory);
                assert!(default.is_none());
            }
            _ => panic!("expected leaf node"),
        }
    }

    // ── extension grammar tests ────────────────────────────────────────────

    fn registry_with_grammar(rules: &'static [crate::grammar::ExtensionGrammar]) -> ModuleRegistry {
        let mut reg = ModuleRegistry::default();
        reg.grammar.register(rules);
        reg
    }

    #[test]
    fn extension_instances_collected_on_leaf() {
        let stmt = parse_module(
            r#"
module ext-test {
  namespace "urn:ext-test";
  prefix et;

  extension purpose {
    argument text;
  }

  leaf mynode {
    type string;
    et:purpose "serves as an example";
  }
}
"#,
        );

        let compiled = compile_module(
            &ModuleKey::latest("ext-test"),
            stmt,
            &ModuleRegistry::default(),
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
        );

        assert!(compiled.errors.is_empty(), "errors: {:?}", compiled.errors);
        let leaf = &compiled.children[0];
        assert_eq!(leaf.extensions.len(), 1);
        let ext = &leaf.extensions[0];
        assert_eq!(ext.module, "et"); // own prefix fallback (module name not in prefix_map)
        assert_eq!(ext.name, "purpose");
        assert_eq!(ext.arg.as_deref(), Some("serves as an example"));
    }

    #[test]
    fn extension_instances_collected_via_import() {
        // Module A defines an extension; module B imports A and uses the extension.
        let a_stmt = parse_module(
            r#"
module ext-defs {
  namespace "urn:ext-defs";
  prefix ed;
  extension callpoint { argument name; }
}
"#,
        );
        let mut reg = ModuleRegistry::default();
        let compiled_a = compile_module(
            &ModuleKey::latest("ext-defs"),
            a_stmt,
            &reg,
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
        );
        reg.insert(std::sync::Arc::new(compiled_a));

        let b_stmt = parse_module(
            r#"
module uses-ext {
  namespace "urn:uses-ext";
  prefix ue;
  import ext-defs { prefix ed; }

  container cfg {
    ed:callpoint "cfg-cp";
  }
}
"#,
        );
        let compiled_b = compile_module(
            &ModuleKey::latest("uses-ext"),
            b_stmt,
            &reg,
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
        );

        assert!(
            compiled_b.errors.is_empty(),
            "errors: {:?}",
            compiled_b.errors
        );
        let container = &compiled_b.children[0];
        assert_eq!(container.name, "cfg");
        assert_eq!(container.extensions.len(), 1);
        let ext = &container.extensions[0];
        assert_eq!(ext.module, "ext-defs");
        assert_eq!(ext.name, "callpoint");
        assert_eq!(ext.arg.as_deref(), Some("cfg-cp"));
    }

    #[test]
    fn extension_instance_accessor_helper() {
        let a_stmt = parse_module(
            r#"
module ext-defs {
  namespace "urn:ext-defs";
  prefix ed;
  extension marker { }
}
"#,
        );
        let mut reg = ModuleRegistry::default();
        let ca = compile_module(
            &ModuleKey::latest("ext-defs"),
            a_stmt,
            &reg,
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
        );
        reg.insert(std::sync::Arc::new(ca));

        let b_stmt = parse_module(
            r#"
module use-marker {
  namespace "urn:use-marker";
  prefix um;
  import ext-defs { prefix ed; }
  leaf x { type string; ed:marker; }
}
"#,
        );
        let cb = compile_module(
            &ModuleKey::latest("use-marker"),
            b_stmt,
            &reg,
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
        );
        let leaf = &cb.children[0];
        assert!(leaf.extension("ext-defs", "marker").is_some());
        assert!(leaf.extension("ext-defs", "other").is_none());
        assert!(leaf.extension("other-mod", "marker").is_none());
    }

    #[test]
    fn grammar_validation_rejects_unexpected_arg() {
        use crate::grammar::ExtensionGrammar;

        static RULES: &[ExtensionGrammar] = &[ExtensionGrammar {
            module: "ext-defs",
            name: "no-arg-ext",
            parents: &[],
            arg: None, // no argument expected
            substmts: &[],
        }];

        let a_stmt = parse_module(
            r#"
module ext-defs {
  namespace "urn:ext-defs";
  prefix ed;
  extension no-arg-ext { }
}
"#,
        );
        let mut reg = registry_with_grammar(RULES);
        let ca = compile_module(
            &ModuleKey::latest("ext-defs"),
            a_stmt,
            &reg,
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
        );
        reg.insert(std::sync::Arc::new(ca));

        let b_stmt = parse_module(
            r#"
module bad-use {
  namespace "urn:bad-use";
  prefix bu;
  import ext-defs { prefix ed; }
  leaf x { type string; ed:no-arg-ext "unexpected"; }
}
"#,
        );
        let cb = compile_module(
            &ModuleKey::latest("bad-use"),
            b_stmt,
            &reg,
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
        );
        // Should produce a validation error about unexpected argument.
        assert!(
            cb.errors
                .iter()
                .any(|e| e.message.contains("does not accept an argument")),
            "expected 'does not accept an argument' error, got: {:?}",
            cb.errors
        );
    }

    #[test]
    fn grammar_validation_rejects_missing_required_substmt() {
        use crate::grammar::{ArgType, Cardinality, ExtensionGrammar, GrammarKeyword, SubstmtSpec};

        static RULES: &[ExtensionGrammar] = &[ExtensionGrammar {
            module: "ext-defs",
            name: "must-have-desc",
            parents: &[],
            arg: Some(ArgType::String),
            substmts: &[SubstmtSpec {
                keyword: GrammarKeyword::BuiltIn(BuiltInKeyword::Description),
                cardinality: Cardinality::Required,
            }],
        }];

        let a_stmt = parse_module(
            r#"
module ext-defs {
  namespace "urn:ext-defs";
  prefix ed;
  extension must-have-desc { argument text; }
}
"#,
        );
        let mut reg = registry_with_grammar(RULES);
        let ca = compile_module(
            &ModuleKey::latest("ext-defs"),
            a_stmt,
            &reg,
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
        );
        reg.insert(std::sync::Arc::new(ca));

        let b_stmt = parse_module(
            r#"
module bad-use {
  namespace "urn:bad-use";
  prefix bu;
  import ext-defs { prefix ed; }
  leaf x {
    type string;
    ed:must-have-desc "title";
    // missing description sub-statement
  }
}
"#,
        );
        let cb = compile_module(
            &ModuleKey::latest("bad-use"),
            b_stmt,
            &reg,
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
        );
        assert!(
            cb.errors
                .iter()
                .any(|e| e.message.contains("requires sub-statement")),
            "expected 'requires sub-statement' error, got: {:?}",
            cb.errors
        );
    }

    #[test]
    fn if_feature_cross_module_eval_uses_resolved_module_name() {
        let mut prefix_map = PrefixMap::new();
        prefix_map.insert("other".to_string(), "other-mod".to_string());
        let pos = Pos::new(Arc::from("test.yang"), 1);
        let mut errors = Vec::new();
        let expr = parse_if_feature_expr(
            "other:feat",
            "self",
            "self-mod",
            &prefix_map,
            &pos,
            &mut errors,
            false,
        )
        .expect("if-feature expression should parse");
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");

        let reg = ModuleRegistry::default();
        let enabled =
            std::collections::HashSet::from([("self-mod".to_string(), "feat".to_string())]);
        let ctx = ExpansionCtx::new(&reg, &enabled);
        assert!(!ctx.eval_if_feature(&expr, "self-mod"));

        let enabled =
            std::collections::HashSet::from([("other-mod".to_string(), "feat".to_string())]);
        let ctx = ExpansionCtx::new(&reg, &enabled);
        assert!(ctx.eval_if_feature(&expr, "self-mod"));
    }

    #[test]
    fn if_feature_unknown_prefix_can_be_ignored() {
        let pos = Pos::new(Arc::from("test.yang"), 1);
        let prefix_map = PrefixMap::new();

        let mut errors = Vec::new();
        let expr = parse_if_feature_expr(
            "missing:feat",
            "self",
            "self-mod",
            &prefix_map,
            &pos,
            &mut errors,
            false,
        )
        .expect("if-feature expression should parse");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("unknown prefix 'missing'")),
            "expected unknown prefix error, got: {errors:?}"
        );
        let reg = ModuleRegistry::default();
        let ctx = ExpansionCtx::all_features(&reg);
        assert!(!ctx.eval_if_feature(&expr, "self-mod"));

        let mut ignored_errors = Vec::new();
        let ignored_expr = parse_if_feature_expr(
            "missing:feat",
            "self",
            "self-mod",
            &prefix_map,
            &pos,
            &mut ignored_errors,
            true,
        )
        .expect("if-feature expression should parse");
        assert!(ignored_errors.is_empty());
        assert!(!ctx.eval_if_feature(&ignored_expr, "self-mod"));
    }
}
