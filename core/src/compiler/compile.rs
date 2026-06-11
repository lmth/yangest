// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
use std::collections::HashMap;
use std::sync::Arc;

use crate::annindex::{AnnotationIndex, PendingAnnotation};
use crate::ast::{BuiltInKeyword, ErrorCode, Keyword, Level, ModuleKey, Pos, Stmt, YError};
use crate::astannindex::AstAnnotationIndex;
use crate::devindex::{DeviationIndex, PendingDeviation};
use crate::grammar::GrammarRegistry;

use super::expansion::attach_schema_path;
use super::{
    AppliedAnnotations, AppliedDeviations, AugmentEntry, CompilationFlags, CompiledModule,
    ExpansionCtx, ExtensionInstance, Feature, Grouping, Identity, IfFeatureExpr,
    LocalAugmentEntry, ModuleRegistry, MustExpr, NodeOverlay, NodeOverlayMap, OrderedBy,
    PathStep, PrefixMap, SchemaNode, SchemaNodeKind, SchemaPath, Status, Typedef,
    UsesOverlay, WhenExpr, YangVersion,
};
use indexmap::IndexMap;

struct NodeCommon {
    name: String,
    pos: Pos,
    status: Status,
    status_pos: Option<Pos>,
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
    ast_ann_index: &AstAnnotationIndex,
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
        &registry.flags,
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
                &grouping.definer_module_name,
                yang_version,
                &grouping.def_own_prefix,
                eff_prefix_map_ref,
                registry,
                &groupings,
                &mut Vec::new(), // ignore grouping-body parse errors here
                ast_ann_index,
            );
            (name.clone(), Arc::new(compiled))
        })
        .collect();

    let mut children = compile_schema_children(
        &stmt.substmts,
        key,
        key.name.as_str(),
        yang_version,
        &prefix,
        &prefix_map,
        registry,
        &groupings,
        &mut module_errors,
        ast_ann_index,
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
        ast_ann_index,
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
        &mut augments,
        ann_index,
        &mut module_errors,
        &mut overlay,
        &grouping_children,
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
        ast_ann_index,
    );

    let mut compiled = CompiledModule {
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
    };
    compiled.set_pdata(collect_applied_deviations(key, dev_index));
    compiled.set_pdata(collect_applied_annotations(key, ann_index));
    compiled
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
    flags: &CompilationFlags,
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
        let opaque_type_name = flags.opaque_type_extension.as_ref().and_then(|(ext_mod, ext_name)| {
            typedef.substmts.iter().find_map(|sub| match &sub.keyword {
                Keyword::Extension { module, name }
                    if module == ext_mod && name == ext_name =>
                {
                    sub.arg.clone()
                }
                Keyword::ExtensionPrefixed { name, .. } if name == ext_name => sub.arg.clone(),
                _ => None,
            })
        });
        let has_opaque_type = opaque_type_name.is_some();
        typedefs.insert(
            name.clone(),
            Typedef {
                name,
                type_stmt,
                units: opt_substmt_arg(typedef, BuiltInKeyword::Units),
                default: opt_substmt_arg(typedef, BuiltInKeyword::Default),
                status: parse_status(typedef, module_errors),
                description: opt_substmt_arg(typedef, BuiltInKeyword::Description),
                ext_info: flags.typedef_info_extension.as_ref().and_then(|(ext_mod, ext_name)| {
                    typedef.substmts.iter().find_map(|sub| {
                        match &sub.keyword {
                            Keyword::Extension { module, name }
                                if module == ext_mod && name == ext_name =>
                            {
                                sub.arg.clone()
                            }
                            Keyword::ExtensionPrefixed { name, .. } if name == ext_name => {
                                sub.arg.clone()
                            }
                            _ => None,
                        }
                    })
                }),
                has_opaque_type,
                opaque_type_name,
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
                scope_groupings: None,
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
    ast_ann_index: &AstAnnotationIndex,
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

        let mut nodes = compile_schema_children(
            &augment.substmts,
            key,
            key.name.as_str(),
            yang_version,
            own_prefix,
            prefix_map,
            registry,
            local_groupings,
            module_errors,
            ast_ann_index,
        );

        // Apply augment-level status to all compiled nodes (RFC 6020 §7.15):
        // "This argument defines the status of all definitions added by this augment."
        let aug_status = augment
            .get_substmt(BuiltInKeyword::Status)
            .map(|s| parse_status_arg(s, module_errors))
            .unwrap_or(Status::Current);
        if aug_status != Status::Current {
            for node in &mut nodes {
                apply_augment_status(node, aug_status);
            }
        }

        augments.push(AugmentEntry {
            target_path,
            nodes,
            when: collect_when_exprs(augment, own_prefix, prefix_map, key.name.as_str(), module_errors, &registry.flags),
            if_features: collect_if_features(
                augment,
                own_prefix,
                &key.name,
                prefix_map,
                module_errors,
                ignore_unknown,
            ),
            status: aug_status,
            pos: augment.pos.clone(),
        });
    }

    // Canonicalise to source order. Augments collected from a single module
    // statement are already in declaration order, so this is a no-op in the
    // common case; the stable sort guarantees the invariant regardless of how
    // the list was assembled (e.g. if submodule augments are ever merged in),
    // and lets every consumer rely on `CompiledModule::augments` being ordered
    // by definition site. Cheap: runs once at compile over a tiny list.
    sort_augments_by_source(&mut augments);

    augments
}

/// Order augments by source position (definition site), so backends can apply
/// them in the same order as the reference compiler. Stable: augments sharing a
/// position keep their relative (declaration) order.
fn sort_augments_by_source(augments: &mut [AugmentEntry]) {
    if augments.len() < 2 {
        return;
    }
    augments.sort_by(|a, b| {
        a.pos
            .orig_file()
            .cmp(b.pos.orig_file())
            .then(a.pos.orig_line().cmp(&b.pos.orig_line()))
    });
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
            | SchemaNodeKind::Notification { children: c, .. } => Arc::make_mut(c).extend(nodes),
            SchemaNodeKind::Choice { cases, .. } => Arc::make_mut(cases).extend(nodes),
            _ => {}
        }
    }
}

fn compile_schema_children(
    stmts: &[Stmt],
    key: &ModuleKey,
    source_module_name: &str,
    yang_version: YangVersion,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    registry: &ModuleRegistry,
    local_groupings: &IndexMap<String, Grouping>,
    module_errors: &mut Vec<YError>,
    ast_ann_index: &AstAnnotationIndex,
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
        // The definer of an inline grouping is the module authoring the enclosing
        // statements (`source_module_name`) — which, inside a grouping body compiled
        // for a `uses`, is the grouping's definer (e.g. a submodule), not the module
        // currently being compiled (`key.name`, the parent). Using `key.name` here
        // mis-attributes `when`/`must` written in a submodule's nested grouping.
        source_module_name,
        module_errors,
    );
    let merged;
    let effective_groupings: &IndexMap<String, Grouping> = if inline_groupings.is_empty() {
        local_groupings
    } else {
        merged = {
            let mut m = local_groupings.clone();
            m.extend(inline_groupings);
            // Store the effective groupings on each inline grouping so that the fallback
            // expansion path (in expand_uses_lazy) can resolve sibling groupings.
            let scope_arc = Arc::new(m.clone());
            for (_, g) in m.iter_mut() {
                if g.scope_groupings.is_none() {
                    g.scope_groupings = Some(Arc::clone(&scope_arc));
                }
            }
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
                    source_module_name,
                    yang_version,
                    own_prefix,
                    prefix_map,
                    registry,
                    effective_groupings,
                    module_errors,
                    ast_ann_index,
                ) {
                    nodes.push(node);
                }
            }
            Some(BuiltInKeyword::Uses) => {
                if let Some(node) = compile_uses_node(
                    stmt,
                    key,
                    source_module_name,
                    yang_version,
                    own_prefix,
                    prefix_map,
                    registry,
                    effective_groupings,
                    module_errors,
                    ast_ann_index,
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
    // Module that authored the enclosing statements (the caller's
    // `source_module_name`). For a `uses` written in a submodule's grouping body
    // this is the submodule, not `key.name` (the parent being compiled) — so
    // `when`/`if-feature` on the `uses` itself, and on its inline `augment`s, are
    // attributed correctly. Distinct from the local `source_module_name` below,
    // which is the *referenced grouping's* source (for resolution/locating).
    enclosing_module_name: &str,
    yang_version: YangVersion,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    registry: &ModuleRegistry,
    local_groupings: &IndexMap<String, Grouping>,
    module_errors: &mut Vec<YError>,
    ast_ann_index: &AstAnnotationIndex,
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
            enclosing_module_name,
            yang_version,
            own_prefix,
            prefix_map,
            registry,
            local_groupings,
            module_errors,
            ast_ann_index,
        );
        local_augments.push(LocalAugmentEntry {
            target_path: target_path.into_iter().map(|step| step.name).collect(),
            nodes,
            when: collect_when_exprs(augment, own_prefix, prefix_map, enclosing_module_name, module_errors, &registry.flags),
            if_features: collect_if_features(
                augment,
                own_prefix,
                enclosing_module_name,
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
        origin_module: key.name.clone(),
        pos: uses_stmt.pos.clone(),
        status: Status::Current,
        status_pos: None, // synthetic `__uses__` placeholder node
        config: None,
        when: Vec::new(),
        if_features: Vec::new(),
        description: None,
        reference: None,
        extensions: Vec::new(),
        kind: SchemaNodeKind::Uses {
            grouping: Arc::new(grouping),
            source_module_name,
            was_unprefixed: grouping_prefix.is_none(),
            overlay: UsesOverlay {
                refine_stmts: uses_stmt
                    .get_substmts(BuiltInKeyword::Refine)
                    .cloned()
                    .collect(),
                local_augments,
                when: collect_when_exprs(uses_stmt, own_prefix, prefix_map, enclosing_module_name, module_errors, &registry.flags),
                if_features: collect_if_features(
                    uses_stmt,
                    own_prefix,
                    enclosing_module_name,
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
    source_module_name: &str,
    yang_version: YangVersion,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    registry: &ModuleRegistry,
    local_groupings: &IndexMap<String, Grouping>,
    module_errors: &mut Vec<YError>,
    ast_ann_index: &AstAnnotationIndex,
) -> Option<SchemaNode> {
    let common = compile_node_common(
        stmt,
        own_prefix,
        source_module_name,
        prefix_map,
        &registry.grammar,
        module_errors,
        registry.flags.ignore_unknown_features,
        &registry.flags,
        ast_ann_index,
    );

    let kind = match builtin_keyword(stmt)? {
        BuiltInKeyword::Container => SchemaNodeKind::Container {
            presence: opt_substmt_arg(stmt, BuiltInKeyword::Presence),
            children: Arc::new(compile_schema_children(
                &stmt.substmts,
                key,
                source_module_name,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
                ast_ann_index,
            )),
            musts: collect_must_exprs(stmt, own_prefix, prefix_map, source_module_name, module_errors, ast_ann_index, &registry.flags),
        },
        BuiltInKeyword::Leaf => SchemaNodeKind::Leaf {
            type_stmt: compile_type_stmt(stmt, yang_version, own_prefix, prefix_map, module_errors),
            units: opt_substmt_arg(stmt, BuiltInKeyword::Units),
            default: opt_substmt_arg(stmt, BuiltInKeyword::Default),
            mandatory: opt_bool_substmt(stmt, BuiltInKeyword::Mandatory, module_errors)
                .unwrap_or(false),
            musts: collect_must_exprs(stmt, own_prefix, prefix_map, source_module_name, module_errors, ast_ann_index, &registry.flags),
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
            musts: collect_must_exprs(stmt, own_prefix, prefix_map, source_module_name, module_errors, ast_ann_index, &registry.flags),
        },
        BuiltInKeyword::List => SchemaNodeKind::List {
            key: parse_key_stmt(stmt.get_substmt(BuiltInKeyword::Key)),
            unique: stmt
                .get_substmts(BuiltInKeyword::Unique)
                .filter_map(|unique| unique.arg.clone())
                .collect(),
            children: Arc::new(compile_schema_children(
                &stmt.substmts,
                key,
                source_module_name,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
                ast_ann_index,
            )),
            min_elements: opt_u64_substmt(stmt, BuiltInKeyword::MinElements, module_errors)
                .unwrap_or(0),
            max_elements: opt_max_elements(stmt, module_errors),
            ordered_by: opt_ordered_by(stmt, module_errors).unwrap_or(OrderedBy::System),
            musts: collect_must_exprs(stmt, own_prefix, prefix_map, source_module_name, module_errors, ast_ann_index, &registry.flags),
        },
        BuiltInKeyword::Choice => SchemaNodeKind::Choice {
            default: opt_substmt_arg(stmt, BuiltInKeyword::Default),
            mandatory: opt_bool_substmt(stmt, BuiltInKeyword::Mandatory, module_errors)
                .unwrap_or(false),
            cases: compile_choice_cases(
                stmt,
                key,
                source_module_name,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
                ast_ann_index,
            ),
        },
        BuiltInKeyword::Case => SchemaNodeKind::Case {
            children: Arc::new(compile_schema_children(
                &stmt.substmts,
                key,
                source_module_name,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
                ast_ann_index,
            )),
        },
        BuiltInKeyword::Rpc => SchemaNodeKind::Rpc {
            input: compile_io_block(
                stmt.get_substmt(BuiltInKeyword::Input),
                key,
                source_module_name,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
                ast_ann_index,
            ),
            output: compile_io_block(
                stmt.get_substmt(BuiltInKeyword::Output),
                key,
                source_module_name,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
                ast_ann_index,
            ),
            musts: collect_must_exprs(stmt, own_prefix, prefix_map, source_module_name, module_errors, ast_ann_index, &registry.flags),
        },
        BuiltInKeyword::Action => SchemaNodeKind::Action {
            input: compile_io_block(
                stmt.get_substmt(BuiltInKeyword::Input),
                key,
                source_module_name,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
                ast_ann_index,
            ),
            output: compile_io_block(
                stmt.get_substmt(BuiltInKeyword::Output),
                key,
                source_module_name,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
                ast_ann_index,
            ),
        },
        BuiltInKeyword::Notification => SchemaNodeKind::Notification {
            children: Arc::new(compile_schema_children(
                &stmt.substmts,
                key,
                source_module_name,
                yang_version,
                own_prefix,
                prefix_map,
                registry,
                local_groupings,
                module_errors,
                ast_ann_index,
            )),
            musts: collect_must_exprs(stmt, own_prefix, prefix_map, source_module_name, module_errors, ast_ann_index, &registry.flags),
        },
        BuiltInKeyword::AnyXml => SchemaNodeKind::AnyXml {
            mandatory: opt_bool_substmt(stmt, BuiltInKeyword::Mandatory, module_errors)
                .unwrap_or(false),
            musts: collect_must_exprs(stmt, own_prefix, prefix_map, source_module_name, module_errors, ast_ann_index, &registry.flags),
        },
        BuiltInKeyword::AnyData => SchemaNodeKind::AnyData {
            mandatory: opt_bool_substmt(stmt, BuiltInKeyword::Mandatory, module_errors)
                .unwrap_or(false),
            musts: collect_must_exprs(stmt, own_prefix, prefix_map, source_module_name, module_errors, ast_ann_index, &registry.flags),
        },
        _ => return None,
    };

    Some(SchemaNode {
        name: common.name,
        module_name: key.name.clone(),
        module_prefix: own_prefix.to_string(),
        origin_module: source_module_name.to_string(),
        pos: common.pos,
        status: common.status,
        status_pos: common.status_pos,
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
    source_module_name: &str,
    yang_version: YangVersion,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    registry: &ModuleRegistry,
    local_groupings: &IndexMap<String, Grouping>,
    module_errors: &mut Vec<YError>,
    ast_ann_index: &AstAnnotationIndex,
) -> Arc<Vec<SchemaNode>> {
    let mut cases = Vec::new();

    for sub in &stmt.substmts {
        match builtin_keyword(sub) {
            Some(BuiltInKeyword::Case) => {
                if let Some(case_node) = compile_schema_node(
                    sub,
                    key,
                    source_module_name,
                    yang_version,
                    own_prefix,
                    prefix_map,
                    registry,
                    local_groupings,
                    module_errors,
                    ast_ann_index,
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
                        source_module_name,
                        yang_version,
                        own_prefix,
                        prefix_map,
                        registry,
                        local_groupings,
                        module_errors,
                        ast_ann_index,
                    )
                    .into_iter()
                    .collect(),
                    _ => compile_schema_node(
                        sub,
                        key,
                        source_module_name,
                        yang_version,
                        own_prefix,
                        prefix_map,
                        registry,
                        local_groupings,
                        module_errors,
                        ast_ann_index,
                    )
                    .into_iter()
                    .collect(),
                };

                let case_name = sub.arg.clone().unwrap_or_else(|| "__case__".to_string());
                cases.push(SchemaNode {
                    name: case_name,
                    module_name: key.name.clone(),
                    module_prefix: own_prefix.to_string(),
                    origin_module: source_module_name.to_string(),
                    pos: sub.pos.clone(),
                    status: Status::Current,
                    // `case` status is not parsed here (kept `current`); no position.
                    status_pos: None,
                    config: None,
                    when: Vec::new(),
                    if_features: Vec::new(),
                    description: None,
                    reference: None,
                    extensions: Vec::new(),
                    kind: SchemaNodeKind::Case { children: Arc::new(children) },
                    pmap: HashMap::new(),
                });
            }
            _ => {}
        }
    }

    Arc::new(cases)
}

fn compile_io_block(
    stmt: Option<&Stmt>,
    key: &ModuleKey,
    source_module_name: &str,
    yang_version: YangVersion,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    registry: &ModuleRegistry,
    local_groupings: &IndexMap<String, Grouping>,
    module_errors: &mut Vec<YError>,
    ast_ann_index: &AstAnnotationIndex,
) -> Arc<Vec<SchemaNode>> {
    stmt.map(|io| {
        Arc::new(compile_schema_children(
            &io.substmts,
            key,
            source_module_name,
            yang_version,
            own_prefix,
            prefix_map,
            registry,
            local_groupings,
            module_errors,
            ast_ann_index,
        ))
    })
    .unwrap_or_default()
}

fn compile_node_common(
    stmt: &Stmt,
    own_prefix: &str,
    source_module_name: &str,
    prefix_map: &PrefixMap,
    grammar: &GrammarRegistry,
    module_errors: &mut Vec<YError>,
    ignore_unknown: bool,
    flags: &CompilationFlags,
    ast_ann_index: &AstAnnotationIndex,
) -> NodeCommon {
    let extensions =
        collect_extension_instances(stmt, own_prefix, prefix_map, grammar, module_errors, ast_ann_index);
    NodeCommon {
        name: required_stmt_name(stmt, module_errors).unwrap_or_default(),
        pos: stmt.pos.clone(),
        status: parse_status(stmt, module_errors),
        status_pos: stmt
            .get_substmt(BuiltInKeyword::Status)
            .map(|s| s.pos.clone()),
        config: opt_bool_substmt(stmt, BuiltInKeyword::Config, module_errors),
        when: collect_when_exprs(stmt, own_prefix, prefix_map, source_module_name, module_errors, flags),
        if_features: collect_if_features(
            stmt,
            own_prefix,
            source_module_name,
            prefix_map,
            module_errors,
            ignore_unknown,
        ),
        description: opt_substmt_arg(stmt, BuiltInKeyword::Description),
        reference: opt_substmt_arg(stmt, BuiltInKeyword::Reference),
        extensions,
    }
}

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
    ast_ann_index: &AstAnnotationIndex,
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

        // If this extension statement was injected by an AST-level annotation
        // (`annotate-module`/`annotate-statement`), its source file maps to the
        // annotation module; record that as the injection source so a backend can
        // attribute the namespace to the annotating module, not the extension's
        // defining module. Normal source extensions map to no annotation file.
        let injection_source_module = ast_ann_index
            .module_key_for_file(sub.pos.orig_file())
            .map(|k| k.name.clone());

        result.push(ExtensionInstance {
            module,
            name,
            arg: sub.arg.clone(),
            substmts: sub.substmts.clone(),
            pos: sub.pos.clone(),
            injection_source_module,
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
    ast_ann_index: &AstAnnotationIndex,
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

        // If this extension statement was injected by an AST-level annotation
        // (`annotate-module`/`annotate-statement`), its source file maps to the
        // annotation module; record that as the injection source so a backend can
        // attribute the namespace to the annotating module, not the extension's
        // defining module. Normal source extensions map to no annotation file.
        let injection_source_module = ast_ann_index
            .module_key_for_file(sub.pos.orig_file())
            .map(|k| k.name.clone());

        result.push(ExtensionInstance {
            module,
            name,
            arg: sub.arg.clone(),
            substmts: sub.substmts.clone(),
            pos: sub.pos.clone(),
            injection_source_module,
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
    source_module_name: &str,
    module_errors: &mut Vec<YError>,
    flags: &CompilationFlags,
) -> Vec<WhenExpr> {
    stmt.get_substmts(BuiltInKeyword::When)
        .map(|when| compile_when_expr(when, own_prefix, prefix_map, source_module_name, module_errors, flags))
        .collect()
}

fn compile_when_expr(
    stmt: &Stmt,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    source_module_name: &str,
    module_errors: &mut Vec<YError>,
    flags: &CompilationFlags,
) -> WhenExpr {
    let xpath = stmt.arg.clone().unwrap_or_default();
    validate_xpath(&xpath, own_prefix, prefix_map, &stmt.pos, module_errors);
    let (explicit_deps, override_auto_deps) =
        collect_explicit_deps(&stmt.substmts, own_prefix, source_module_name, prefix_map, flags);
    WhenExpr {
        xpath,
        description: opt_substmt_arg(stmt, BuiltInKeyword::Description),
        reference: opt_substmt_arg(stmt, BuiltInKeyword::Reference),
        source_module: source_module_name.to_string(),
        source_revision: None,
        non_local: false,
        explicit_deps,
        override_auto_deps,
    }
}

fn collect_must_exprs(
    stmt: &Stmt,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    source_module_name: &str,
    module_errors: &mut Vec<YError>,
    ast_ann_index: &AstAnnotationIndex,
    flags: &CompilationFlags,
) -> Vec<MustExpr> {
    stmt.get_substmts(BuiltInKeyword::Must)
        .map(|must| compile_must_expr(must, own_prefix, prefix_map, source_module_name, module_errors, ast_ann_index, flags))
        .collect()
}

fn compile_must_expr(
    stmt: &Stmt,
    own_prefix: &str,
    prefix_map: &PrefixMap,
    source_module_name: &str,
    module_errors: &mut Vec<YError>,
    ast_ann_index: &AstAnnotationIndex,
    flags: &CompilationFlags,
) -> MustExpr {
    let xpath = stmt.arg.clone().unwrap_or_default();
    validate_xpath(&xpath, own_prefix, prefix_map, &stmt.pos, module_errors);
    // Check if this must statement was injected from an annotation module (annotate-module +
    // annotate-statement). If so, the stmt's source file differs from the target module file,
    // and ast_ann_index.module_key_for_file() returns the annotation module's key.
    let (source_module, source_revision) =
        match ast_ann_index.module_key_for_file(stmt.pos.orig_file()) {
            Some(ann_key) => (ann_key.name.clone(), ann_key.revision.clone()),
            None => (source_module_name.to_string(), None),
        };
    let (explicit_deps, override_auto_deps) =
        collect_explicit_deps(&stmt.substmts, own_prefix, source_module_name, prefix_map, flags);
    MustExpr {
        xpath,
        error_message: opt_substmt_arg(stmt, BuiltInKeyword::ErrorMessage),
        error_app_tag: opt_substmt_arg(stmt, BuiltInKeyword::ErrorAppTag),
        description: opt_substmt_arg(stmt, BuiltInKeyword::Description),
        source_module,
        source_revision,
        explicit_deps,
        override_auto_deps,
    }
}

/// Resolve the configured `dependency` and `override-auto-dependencies`
/// extensions (if any) inside a `must` or `when` statement's sub-statements,
/// using the lookup keys configured in [`CompilationFlags`].
///
/// Returns `(explicit_deps, override_auto_deps)`. When the corresponding
/// extension is unset in `flags`, the result is empty / `false` (the default —
/// no built-in name strings are matched).
fn collect_explicit_deps(
    substmts: &[Stmt],
    own_prefix: &str,
    source_module_name: &str,
    prefix_map: &PrefixMap,
    flags: &CompilationFlags,
) -> (Vec<String>, bool) {
    let explicit_deps: Vec<String> = if let Some((mod_name, ext_name)) = flags.dependency_extension.as_ref() {
        substmts
            .iter()
            .filter(|sub| {
                resolve_kw(&sub.keyword, own_prefix, source_module_name, prefix_map)
                    .map(|(m, n)| m == mod_name.as_str() && n == ext_name.as_str())
                    .unwrap_or(false)
            })
            .filter_map(|sub| sub.arg.clone())
            .collect()
    } else {
        Vec::new()
    };
    let override_auto_deps = if let Some((mod_name, ext_name)) = flags.override_auto_deps_extension.as_ref() {
        substmts.iter().any(|sub| {
            resolve_kw(&sub.keyword, own_prefix, source_module_name, prefix_map)
                .map(|(m, n)| m == mod_name.as_str() && n == ext_name.as_str())
                .unwrap_or(false)
        })
    } else {
        false
    };
    (explicit_deps, override_auto_deps)
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
    expand_children_inner(raw, own_prefix, module_name, overlay, None, parent_path, ctx)
}

/// Like [`expand_children`] but with an additional secondary overlay that is checked
/// when the primary overlay does not contain an entry for a given key.  Used when
/// expanding children of an external-grouping node where annotations may come from
/// both the node's defining module and the file module being emitted.
pub fn expand_children_with_secondary(
    raw: &[SchemaNode],
    own_prefix: &str,
    module_name: &str,
    overlay: &NodeOverlayMap,
    secondary_overlay: &NodeOverlayMap,
    parent_path: &[PathStep],
    ctx: &ExpansionCtx<'_>,
) -> Vec<SchemaNode> {
    expand_children_inner(raw, own_prefix, module_name, overlay, Some(secondary_overlay), parent_path, ctx)
}

fn expand_children_inner(
    raw: &[SchemaNode],
    own_prefix: &str,
    module_name: &str,
    overlay: &NodeOverlayMap,
    secondary_overlay: Option<&NodeOverlayMap>,
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
                was_unprefixed: _,
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
                result.extend(expand_children_inner(
                    &expanded,
                    &node.module_prefix,
                    &node.module_name,
                    overlay,
                    secondary_overlay,
                    parent_path,
                    ctx,
                ));
            }
            _ => {
                let has_overlay = ctx.has_any_overlay
                    && (!overlay.is_empty() || secondary_overlay.map_or(false, |s| !s.is_empty()));
                if !has_overlay {
                    // Fast path: no overlay applicable — skip per-node path allocation,
                    // overlay lookup, and schema_path pmap insert.
                    let materialized = node.clone();
                    if node_visible(&materialized, ctx) {
                        result.push(materialized);
                    }
                } else {
                    let mut materialized = node.clone();
                    let overlay_path =
                        overlay_name_path(parent_path, &materialized.name);
                    // Check primary overlay first, then secondary; each tries the
                    // module-qualified key before the unqualified fallback.
                    let entry = lookup_overlay(overlay, &overlay_path, &materialized.module_name)
                        .or_else(|| {
                            secondary_overlay.and_then(|s| {
                                lookup_overlay(s, &overlay_path, &materialized.module_name)
                            })
                        });
                    if !apply_overlay_entry(&mut materialized, entry, ctx) {
                        continue;
                    }
                    if !node_visible(&materialized, ctx) {
                        continue;
                    }
                    let node_path =
                        child_path(parent_path, &materialized.module_prefix, &materialized.name);
                    attach_schema_path(&mut materialized, node_path);
                    result.push(materialized);
                }
            }
        }
    }
    result
}

/// Like [`expand_children`] but skips the if-feature visibility check, including
/// feature-gated nodes in the result.  Used for collecting enum types from all nodes
/// (the full schema tree is walked regardless of if-feature state).
pub fn expand_children_all(
    raw: &[SchemaNode],
    overlay: &NodeOverlayMap,
    ctx: &ExpansionCtx<'_>,
) -> Vec<SchemaNode> {
    let mut result = Vec::with_capacity(raw.len());
    for node in raw {
        match &node.kind {
            SchemaNodeKind::Uses {
                grouping,
                source_module_name,
                was_unprefixed: _,
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
                result.extend(expand_children_all(&expanded, overlay, ctx));
            }
            _ => {
                // Include the node regardless of if-feature evaluation.
                // Nodes with `not-supported` deviations are still excluded.
                let mut materialized = node.clone();
                if ctx.has_any_overlay && !overlay.is_empty() {
                    let overlay_path = overlay_name_path(&[], &materialized.name);
                    let entry = lookup_overlay(overlay, &overlay_path, &materialized.module_name);
                    if !apply_overlay_entry(&mut materialized, entry, ctx) {
                        continue;
                    }
                }
                result.push(materialized);
            }
        }
    }
    result
}

/// Like [`expand_children`] but also collects all nodes (including feature-gated ones)
/// in a single pass.  Returns `(enabled, all)` where `enabled` is filtered by if-feature
/// visibility and `all` includes every non-`not-supported` node regardless of if-features.
///
/// Use this in place of calling `expand_children` + `expand_children_all` separately to
/// avoid expanding Uses groupings twice (even if cached, the node cloning is repeated).
pub fn expand_children_and_all(
    raw: &[SchemaNode],
    own_prefix: &str,
    module_name: &str,
    overlay: &NodeOverlayMap,
    parent_path: &[PathStep],
    ctx: &ExpansionCtx<'_>,
) -> (Vec<SchemaNode>, Vec<SchemaNode>) {
    let _ = (own_prefix, module_name);
    let mut enabled: Vec<SchemaNode> = Vec::with_capacity(raw.len());
    let mut all: Vec<SchemaNode> = Vec::with_capacity(raw.len());
    for node in raw {
        match &node.kind {
            SchemaNodeKind::Uses {
                grouping,
                source_module_name,
                was_unprefixed: _,
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
                let (mut sub_enabled, mut sub_all) = expand_children_and_all(
                    &expanded,
                    &node.module_prefix,
                    &node.module_name,
                    overlay,
                    parent_path,
                    ctx,
                );
                enabled.append(&mut sub_enabled);
                all.append(&mut sub_all);
            }
            _ => {
                if !ctx.has_any_overlay || overlay.is_empty() {
                    // Fast path: no overlay — clone once, push to both if visible.
                    let materialized = node.clone();
                    if node_visible(&materialized, ctx) {
                        // Clone a second time for `all` only when the node is visible
                        // (in the common case where all features are enabled, this is
                        // always true and we avoid a second clone for invisible nodes).
                        all.push(materialized.clone());
                        enabled.push(materialized);
                    } else {
                        // Feature-gated: goes to `all` only.
                        all.push(materialized);
                    }
                } else {
                    let mut materialized = node.clone();
                    let overlay_path = overlay_name_path(parent_path, &materialized.name);
                    let entry = lookup_overlay(overlay, &overlay_path, &materialized.module_name);
                    if !apply_overlay_entry(&mut materialized, entry, ctx) {
                        // `not-supported` deviated — exclude from both.
                        continue;
                    }
                    if node_visible(&materialized, ctx) {
                        let node_path =
                            child_path(parent_path, &materialized.module_prefix, &materialized.name);
                        attach_schema_path(&mut materialized, node_path.clone());
                        // pmap is wiped by clone(), so re-attach the schema_path to the clone
                        // going into `all`. Nodes in `all` are iterated by collect_types_forward
                        // via all_children() and need the path for deviation overlay lookups.
                        let mut for_all = materialized.clone();
                        attach_schema_path(&mut for_all, node_path);
                        all.push(for_all);
                        enabled.push(materialized);
                    } else {
                        // Feature-gated: still in `all` but not in `enabled`.
                        all.push(materialized);
                    }
                }
            }
        }
    }
    (enabled, all)
}

fn child_path(parent_path: &[PathStep], module_prefix: &str, name: &str) -> SchemaPath {
    let mut path = parent_path.to_vec();
    path.push(PathStep {
        prefix: Some(module_prefix.to_string()),
        name: name.to_string(),
    });
    path
}

/// Build a name-only overlay path (no prefixes) from a parent path and a child
/// name. Prefixes are omitted so that nodes from expanded groupings (which may
/// retain their source module's prefix) still match deviation/annotation paths
/// that use the importing module's prefix; module disambiguation is layered on
/// top via the [`OverlayKey`]'s optional leaf-module qualifier.
pub fn overlay_name_path(parent_key: &[PathStep], name: &str) -> Vec<String> {
    let mut path: Vec<String> = parent_key.iter().map(|s| s.name.clone()).collect();
    path.push(name.to_string());
    path
}

/// Look up an overlay entry for a node, trying the module-qualified key first
/// (so same-named siblings from different modules don't collide) and falling
/// back to the unqualified key (preserving grouping-expansion matches where the
/// node's module differs from the path prefix).
fn lookup_overlay<'a>(
    map: &'a NodeOverlayMap,
    path: &[String],
    node_module: &str,
) -> Option<&'a NodeOverlay> {
    map.get(&(path.to_vec(), Some(node_module.to_string())))
        .or_else(|| map.get(&(path.to_vec(), None)))
}

/// Insert into / update an overlay under both the module-qualified key (when the
/// leaf module is known) and the unqualified key, so qualified lookups
/// disambiguate while unqualified lookups still match. `update` is applied to
/// each bucket's [`NodeOverlay`].
fn overlay_entry_mut<F: FnMut(&mut NodeOverlay)>(
    overlay: &mut NodeOverlayMap,
    path: Vec<String>,
    leaf_module: Option<String>,
    mut update: F,
) {
    if let Some(m) = leaf_module {
        update(overlay.entry((path.clone(), Some(m))).or_default());
    }
    update(overlay.entry((path, None)).or_default());
}

/// Find a named child in a raw node list with early termination, expanding
/// `Uses` groupings lazily as needed.
///
/// This is an early-termination variant of [`expand_children`]: it stops as
/// soon as the first visible child with `name == target_name` is found rather
/// than expanding all children.  Useful for navigating augment target paths
/// step-by-step in large modules without the O(n) cost of full expansion.
pub fn find_child_in_raw(
    target_name: &str,
    raw: &[SchemaNode],
    overlay: &NodeOverlayMap,
    ctx: &ExpansionCtx<'_>,
) -> Option<SchemaNode> {
    let empty_overlay = NodeOverlayMap::new();
    for node in raw {
        match &node.kind {
            SchemaNodeKind::Uses { grouping, source_module_name, overlay: uses_overlay, was_unprefixed: _ } => {
                let expanded = expand_uses_lazy(
                    grouping,
                    source_module_name.as_deref(),
                    uses_overlay,
                    &node.module_prefix,
                    &node.module_name,
                    &empty_overlay,
                    ctx,
                );
                if let Some(found) = find_child_in_raw(target_name, &expanded, overlay, ctx) {
                    return Some(found);
                }
            }
            _ => {
                if node.name == target_name && node_visible(node, ctx) {
                    return Some(node.clone());
                }
            }
        }
    }
    None
}

/// Walk a YANG path through a node-slice without ever cloning intermediate nodes.
///
/// For each step in `path`, finds the matching child by name (expanding `Uses`
/// lazily via the ctx cache but keeping the Arc alive on the call stack so no
/// `SchemaNode` clone is needed).  When the final step is found the closure
/// `f` is called with a reference to that terminal node; the closure's return
/// value is propagated back.
///
/// Returns `None` only when a path step is not found.
///
/// This is the performance-critical alternative to calling `find_child_in_raw`
/// step-by-step (which clones every intermediate node, potentially deep-cloning
/// subtrees with thousands of fully-expanded descendants).
pub fn walk_path_no_clone<R>(
    path: &[PathStep],
    raw: &[SchemaNode],
    ctx: &ExpansionCtx<'_>,
    f: &impl Fn(&SchemaNode) -> R,
) -> Option<R> {
    let Some(first) = path.first() else { return None };
    let rest = &path[1..];
    let empty_overlay = NodeOverlayMap::new();

    for node in raw {
        match &node.kind {
            SchemaNodeKind::Uses { grouping, source_module_name, overlay: uses_overlay, was_unprefixed: _ } => {
                let expanded = expand_uses_lazy(
                    grouping,
                    source_module_name.as_deref(),
                    uses_overlay,
                    &node.module_prefix,
                    &node.module_name,
                    &empty_overlay,
                    ctx,
                );
                // Arc kept alive on the call stack — no clone needed.
                if let Some(r) = walk_path_no_clone(path, &expanded, ctx, f) {
                    return Some(r);
                }
            }
            _ => {
                if node.name == first.name && node_visible(node, ctx) {
                    if rest.is_empty() {
                        return Some(f(node));
                    }
                    // Recurse into this node's raw children.
                    if let Some(children) = raw_children_of_kind(&node.kind) {
                        return walk_path_no_clone(rest, children, ctx, f);
                    }
                    return None;
                }
            }
        }
    }
    None
}

/// Returns the raw child slice for a schema node kind, or `None` for leaf/leaf-list/rpc-io/anyxml/anydata.
pub fn raw_children_of_kind(kind: &SchemaNodeKind) -> Option<&[SchemaNode]> {
    match kind {
        SchemaNodeKind::Container { children, .. }
        | SchemaNodeKind::Case { children, .. }
        | SchemaNodeKind::Notification { children, .. } => Some(children),
        SchemaNodeKind::Choice { cases, .. } => Some(cases),
        SchemaNodeKind::List { children, .. } => Some(children),
        _ => None,
    }
}

/// Like [`walk_path_no_clone`], but the closure `f` is also given the `raw`
/// slice that contains the terminal node (i.e., the terminal node's siblings).
/// This allows the caller to do further relative navigation from the parent.
pub fn walk_path_with_siblings<R>(
    path: &[PathStep],
    raw: &[SchemaNode],
    ctx: &ExpansionCtx<'_>,
    f: &impl Fn(&SchemaNode, &[SchemaNode]) -> R,
) -> Option<R> {
    let Some(first) = path.first() else { return None };
    let rest = &path[1..];
    let empty_overlay = NodeOverlayMap::new();

    for node in raw {
        match &node.kind {
            SchemaNodeKind::Uses { grouping, source_module_name, overlay: uses_overlay, was_unprefixed: _ } => {
                let expanded = expand_uses_lazy(
                    grouping,
                    source_module_name.as_deref(),
                    uses_overlay,
                    &node.module_prefix,
                    &node.module_name,
                    &empty_overlay,
                    ctx,
                );
                if let Some(r) = walk_path_with_siblings(path, &expanded, ctx, f) {
                    return Some(r);
                }
            }
            _ => {
                if node.name == first.name && node_visible(node, ctx) {
                    if rest.is_empty() {
                        return Some(f(node, raw));
                    }
                    if let Some(children) = raw_children_of_kind(&node.kind) {
                        return walk_path_with_siblings(rest, children, ctx, f);
                    }
                    return None;
                }
            }
        }
    }
    None
}

/// Walk a YANG path, folding a `Copy` accumulator over every matched node
/// without ever cloning any `SchemaNode`.
///
/// Like [`walk_path_no_clone`], but the closure `f` is called for **each**
/// matched step (not only the terminal), allowing the caller to accumulate
/// per-step information (e.g., the last-explicit `config` value along a path).
///
/// `f(acc, node)` is called with the current accumulator and the matched
/// node reference; the returned value becomes the new accumulator.
///
/// Returns `None` only if a path step is not found.
pub fn fold_path_no_clone<S, F>(
    path: &[PathStep],
    raw: &[SchemaNode],
    ctx: &ExpansionCtx<'_>,
    init: S,
    f: &F,
) -> Option<S>
where
    S: Copy,
    F: Fn(S, &SchemaNode) -> S,
{
    let Some(first) = path.first() else { return Some(init) };
    let rest = &path[1..];
    let empty_overlay = NodeOverlayMap::new();

    for node in raw {
        match &node.kind {
            SchemaNodeKind::Uses { grouping, source_module_name, overlay: uses_overlay, was_unprefixed: _ } => {
                let expanded = expand_uses_lazy(
                    grouping,
                    source_module_name.as_deref(),
                    uses_overlay,
                    &node.module_prefix,
                    &node.module_name,
                    &empty_overlay,
                    ctx,
                );
                // Arc kept alive on the call stack — no SchemaNode clone needed.
                if let Some(r) = fold_path_no_clone(path, &expanded, ctx, init, f) {
                    return Some(r);
                }
            }
            _ => {
                if node.name == first.name && node_visible(node, ctx) {
                    let new_acc = f(init, node);
                    if rest.is_empty() {
                        return Some(new_acc);
                    }
                    if let Some(children) = raw_children_of_kind(&node.kind) {
                        return fold_path_no_clone(rest, children, ctx, new_acc, f);
                    }
                    return None;
                }
            }
        }
    }
    None
}


/// Walk a YANG path through raw schema nodes, collecting every intermediate
/// node (all matched steps except the terminal) as owned clones.
///
/// Returns `Some((terminal_clone, intermediates))` where:
/// - `terminal_clone` is an owned clone of the terminal node, and
/// - `intermediates` is a `Vec<SchemaNode>` of clones of all nodes visited
///   before the terminal, in root-to-parent order (the parent is last).
///
/// Returns `None` if any step in the path cannot be found.
///
/// Handles `Uses` expansion transparently (same as `walk_path_with_siblings`).
///
/// Each level is materialised exactly as the main expansion path
/// ([`expand_children_inner`]) does — expanding `uses` *and* applying the
/// deviation/annotation overlay with correct path keys — so a deviation deferred
/// past a `uses` (stashed in the module overlay because `apply_deviations`
/// couldn't descend into the `Uses` node) is observed identically to the cs
/// records the main path emits. An earlier version special-cased `Uses` with an
/// empty overlay and an untracked parent path, so the overlay key for a node
/// reached through a `uses` lost its ancestor prefix and the deferred deviation
/// never matched.
pub fn walk_path_collecting_intermediates(
    path: &[PathStep],
    raw: &[SchemaNode],
    ctx: &ExpansionCtx<'_>,
) -> Option<(SchemaNode, Vec<SchemaNode>)> {
    if path.is_empty() {
        return None;
    }
    let empty_overlay = NodeOverlayMap::new();
    // Seed from the module the walk is rooted in (the consuming module that owns
    // any deviation deferred past a `uses`), mirroring `CompiledModule::children`:
    // `raw` is that module's child slice. This rooted overlay is threaded as the
    // *secondary* at every level so a deferred deviation is still found after the
    // descent crosses into a foreign grouping body (whose own module overlay is
    // empty) — the same role `file_module_overlay` plays for the main path.
    let root_owner = raw
        .first()
        .and_then(|n| ctx.registry.resolve_import(&n.module_name, None));
    let root_overlay = root_owner.as_ref().map(|m| &m.overlay).unwrap_or(&empty_overlay);
    walk_collecting_overlay(path, raw, root_overlay, root_overlay, &[], ctx)
}

/// Inner driver for [`walk_path_collecting_intermediates`] that threads the active
/// `primary` overlay (the current level's residence-module overlay), the rooted
/// `deferred` overlay (secondary, where deviations stashed past a `uses` live),
/// and the accumulated `parent_path` — matching `SchemaNode::children` semantics.
fn walk_collecting_overlay(
    path: &[PathStep],
    raw: &[SchemaNode],
    primary: &NodeOverlayMap,
    deferred: &NodeOverlayMap,
    parent_path: &[PathStep],
    ctx: &ExpansionCtx<'_>,
) -> Option<(SchemaNode, Vec<SchemaNode>)> {
    let first = &path[0];
    let rest = &path[1..];
    let empty_overlay = NodeOverlayMap::new();

    // Secondary overlay: prefer an explicitly-set `file_module_overlay` (the
    // emitter's contract), else fall back to the rooted `deferred` overlay so the
    // walker is correct stand-alone.
    let fallback_ptr = ctx.file_module_overlay.get();
    let secondary: &NodeOverlayMap = if fallback_ptr.is_null() {
        deferred
    } else {
        // SAFETY: same contract as `SchemaNode::children` — the pointer is set by the
        // emitter immediately before walking and points to a `CompiledModule` overlay
        // that outlives this call.
        unsafe { &*fallback_ptr }
    };

    // Materialise this level the way the main path does: `uses` expanded
    // transparently, overlay applied per node with `parent_path`-derived keys.
    let level = expand_children_inner(raw, "", "", primary, Some(secondary), parent_path, ctx);
    let node = level.into_iter().find(|n| n.name == first.name)?;
    if rest.is_empty() {
        return Some((node, vec![]));
    }
    let raw_children = node.raw_children()?;
    // Next level's primary overlay = the matched node's residence-module overlay,
    // and `parent_path` = its (now-attached) schema path — both as `children` does.
    let next_owner = ctx.registry.resolve_import(&node.module_name, None);
    let next_primary = next_owner.as_ref().map(|m| &m.overlay).unwrap_or(&empty_overlay);
    let next_parent = node.schema_path().unwrap_or_else(|| {
        vec![PathStep { prefix: Some(node.module_prefix.clone()), name: node.name.clone() }]
    });
    let (terminal, mut intermediates) =
        walk_collecting_overlay(rest, raw_children, next_primary, deferred, &next_parent, ctx)?;
    intermediates.insert(0, node);
    Some((terminal, intermediates))
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

fn apply_overlay_entry(
    node: &mut SchemaNode,
    overlay: Option<&NodeOverlay>,
    ctx: &ExpansionCtx<'_>,
) -> bool {
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

    // The deviate body's extension prefixes (e.g. an explicit-dependency marker on
    // a `must`) resolve in the *deviating* module's scope. Resolve that module so
    // its prefix map + the compilation flags reach `apply_node_mutation` — without
    // it, a deviation-added must/when would drop its explicit_deps/override_auto_deps.
    let dev_module = overlay
        .source_module
        .as_deref()
        .and_then(|m| ctx.registry.resolve_import(m, None));
    let dev_prefix_ctx = dev_module.as_ref().map(|m| (m.prefix.as_str(), &m.prefix_map));
    let dev_flags = Some(&ctx.registry.flags);

    let mut ignored_errors = Vec::new();
    for deviate in &overlay.deviate_stmts {
        let mode = match deviate.arg.as_deref() {
            Some("add") => MutationMode::Add,
            Some("replace") => MutationMode::Replace,
            Some("delete") => MutationMode::Delete,
            _ => continue,
        };
        apply_node_mutation(
            node,
            &deviate.substmts,
            mode,
            overlay.source_module.as_deref().unwrap_or(""),
            dev_prefix_ctx,
            false, // deviation, not refine
            false, // if-feature parsing is refine-only; unused for deviations
            dev_flags,
            &mut ignored_errors,
        );
    }

    // Inject annotation extension instances and apply when/must from annotations.
    for ann in &overlay.annotations {
        node.extensions.extend(ann.instances.iter().map(|inst| {
            let mut inst = inst.clone();
            inst.injection_source_module = Some(ann.source_module.clone());
            inst
        }));
        let source = ann.source_module.as_str();
        let source_rev = ann.source_revision.clone();
        for when in &ann.when_stmts {
            mutate_when_exprs(&mut node.when, when, MutationMode::Add, source, source_rev.clone(), None, None);
        }
        match &mut node.kind {
            SchemaNodeKind::Container { musts, .. }
            | SchemaNodeKind::Leaf { musts, .. }
            | SchemaNodeKind::LeafList { musts, .. }
            | SchemaNodeKind::List { musts, .. }
            | SchemaNodeKind::Rpc { musts, .. }
            | SchemaNodeKind::Notification { musts, .. }
            | SchemaNodeKind::AnyXml { musts, .. }
            | SchemaNodeKind::AnyData { musts, .. } => {
                for must in &ann.must_stmts {
                    mutate_must_exprs(musts, must, MutationMode::Add, source, source_rev.clone(), None, None);
                }
            }
            _ => {}
        }
    }

    true
}

/// Recursively set `scope_groupings` on any Uses node's grouping that doesn't already have it.
/// This ensures nested grouping references can resolve sibling groupings transitively.
/// Old Arcs are collected in `keepalive` to prevent memory address reuse which would corrupt
/// the pointer-based expansion cache.
fn propagate_scope_groupings(
    nodes: &mut [SchemaNode],
    scope: &Arc<IndexMap<String, Grouping>>,
) {
    for node in nodes.iter_mut() {
        match &mut node.kind {
            SchemaNodeKind::Uses { grouping, .. } => {
                if grouping.scope_groupings.is_none() {
                    let mut g = (**grouping).clone();
                    g.scope_groupings = Some(Arc::clone(scope));
                    *grouping = Arc::new(g);
                }
            }
            SchemaNodeKind::Container { children, .. }
            | SchemaNodeKind::List { children, .. }
            | SchemaNodeKind::Case { children }
            | SchemaNodeKind::Notification { children, .. } => {
                propagate_scope_groupings(Arc::make_mut(children).as_mut_slice(), scope);
            }
            SchemaNodeKind::Choice { cases, .. } => {
                propagate_scope_groupings(Arc::make_mut(cases).as_mut_slice(), scope);
            }
            SchemaNodeKind::Rpc { input, output, .. }
            | SchemaNodeKind::Action { input, output } => {
                propagate_scope_groupings(Arc::make_mut(input).as_mut_slice(), scope);
                propagate_scope_groupings(Arc::make_mut(output).as_mut_slice(), scope);
            }
            _ => {}
        }
    }
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
                was_unprefixed: _,
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
                        *children = Arc::new(fully_expand_nodes(
                            children,
                            &node.module_prefix,
                            &node.module_name,
                            ctx,
                        ));
                    }
                    SchemaNodeKind::Choice { cases, .. } => {
                        *cases = Arc::new(fully_expand_nodes(
                            cases,
                            &node.module_prefix,
                            &node.module_name,
                            ctx,
                        ));
                    }
                    SchemaNodeKind::Rpc { input, output, .. }
                    | SchemaNodeKind::Action { input, output } => {
                        *input = Arc::new(fully_expand_nodes(
                            input,
                            &node.module_prefix,
                            &node.module_name,
                            ctx,
                        ));
                        *output = Arc::new(fully_expand_nodes(
                            output,
                            &node.module_prefix,
                            &node.module_name,
                            ctx,
                        ));
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

/// Expand a `uses` of `grouping` into module `module_name`.
///
/// Thin wrapper over [`expand_uses_lazy_inner`] that, when
/// [`CompilationFlags::scope_grouping_annotations_to_target`] is set, drops
/// annotation-injected `must`/`when` constraints that belong to an annotation
/// module not targeting the consuming module — so a constraint an
/// `annotate-module "base"` overlay injected into a grouping does not ride into
/// another module that merely reuses the grouping. Off by default; when off this
/// is a zero-cost pass-through that preserves the shared/Arc-cached expansion.
fn expand_uses_lazy(
    grouping: &Arc<Grouping>,
    source_module_name: Option<&str>,
    uses_overlay: &UsesOverlay,
    own_prefix: &str,
    module_name: &str,
    overlay: &NodeOverlayMap,
    ctx: &ExpansionCtx<'_>,
) -> Arc<Vec<SchemaNode>> {
    let expanded = expand_uses_lazy_inner(
        grouping,
        source_module_name,
        uses_overlay,
        own_prefix,
        module_name,
        overlay,
        ctx,
    );
    if !ctx.registry.flags.scope_grouping_annotations_to_target {
        return expanded;
    }
    let Some(ann) = ctx.annotation_index() else {
        return expanded;
    };
    // The scan of a grouping's expansion is a pure function of the grouping and the
    // consuming module, so memoise it (keyed by grouping pointer) instead of
    // re-walking the whole expanded tree on every call. Two levels:
    //   - `has_any`: does the grouping carry ANY annotation-injected statement?
    //     Computed once per grouping; false for the vast majority, which then skip
    //     all per-call work and keep the shared Arc.
    //   - `foreign`: for grouping bodies that do, whether a statement foreign to
    //     this consuming module is present — memoised per consuming module.
    let gptr = Arc::as_ptr(grouping) as usize;

    let has_any = ctx.ann_scope_cache.borrow().get(&gptr).map(|v| v.has_any);
    let has_any = has_any.unwrap_or_else(|| {
        let v = expanded
            .iter()
            .any(|n| node_has_annotation_injected(n, ann));
        ctx.ann_scope_cache.borrow_mut().entry(gptr).or_default().has_any = v;
        v
    });
    if !has_any {
        return expanded;
    }

    let foreign = ctx
        .ann_scope_cache
        .borrow()
        .get(&gptr)
        .and_then(|v| v.foreign.get(module_name).copied());
    let foreign = foreign.unwrap_or_else(|| {
        let v = expanded
            .iter()
            .any(|n| node_has_foreign_annotation(n, module_name, ann));
        ctx.ann_scope_cache
            .borrow_mut()
            .entry(gptr)
            .or_default()
            .foreign
            .insert(module_name.to_string(), v);
        v
    });
    if foreign {
        Arc::new(filter_foreign_annotations(&expanded, module_name, ann))
    } else {
        expanded
    }
}

/// Recursively scan for any annotation-injected `must`/`when`/extension,
/// regardless of which module it targets. Target-independent companion to
/// [`node_has_foreign_annotation`]; used to compute `GroupingScopeVerdict::has_any`.
fn node_has_annotation_injected(node: &SchemaNode, ann: &AstAnnotationIndex) -> bool {
    if node
        .when
        .iter()
        .any(|w| ann.is_annotation_module(&w.source_module))
        || node
            .extensions
            .iter()
            .any(|e| ann.module_key_for_file(e.pos.orig_file()).is_some())
    {
        return true;
    }
    let musts_injected =
        |musts: &[MustExpr]| musts.iter().any(|m| ann.is_annotation_module(&m.source_module));
    let kids_injected =
        |kids: &[SchemaNode]| kids.iter().any(|k| node_has_annotation_injected(k, ann));
    match &node.kind {
        SchemaNodeKind::Container { children, musts, .. }
        | SchemaNodeKind::Notification { children, musts, .. }
        | SchemaNodeKind::List { children, musts, .. } => {
            musts_injected(musts) || kids_injected(children)
        }
        SchemaNodeKind::Leaf { musts, .. }
        | SchemaNodeKind::LeafList { musts, .. }
        | SchemaNodeKind::AnyXml { musts, .. }
        | SchemaNodeKind::AnyData { musts, .. } => musts_injected(musts),
        SchemaNodeKind::Rpc { input, output, musts, .. } => {
            musts_injected(musts) || kids_injected(input) || kids_injected(output)
        }
        SchemaNodeKind::Action { input, output } => {
            kids_injected(input) || kids_injected(output)
        }
        SchemaNodeKind::Case { children } => kids_injected(children),
        SchemaNodeKind::Choice { cases, .. } => kids_injected(cases),
        SchemaNodeKind::Uses { .. } => false,
    }
}

/// True when a `must`/`when` (identified by its already-resolved `source_module`)
/// was injected by an annotation module whose target is not `consuming`.
fn is_foreign_annotation_constraint(
    source_module: &str,
    consuming: &str,
    ann: &AstAnnotationIndex,
) -> bool {
    ann.is_annotation_module(source_module) && !ann.annotation_targets(source_module, consuming)
}

/// True when an extension instance was injected by an annotation module whose
/// target is not `consuming`. Extensions don't carry a resolved source-module the
/// way `must`/`when` do, so provenance comes from the statement's *original* file
/// (`pos.orig_file()`, which survives `uses` expansion) — the same resolution
/// `compile_must_expr` uses to stamp `MustExpr::source_module`.
fn is_foreign_annotation_ext(
    ext: &ExtensionInstance,
    consuming: &str,
    ann: &AstAnnotationIndex,
) -> bool {
    match ann.module_key_for_file(ext.pos.orig_file()) {
        Some(ann_key) => !ann.annotation_targets(&ann_key.name, consuming),
        None => false,
    }
}

/// Recursively scan for any annotation-injected `must`/`when`/extension foreign to
/// `consuming`.
fn node_has_foreign_annotation(
    node: &SchemaNode,
    consuming: &str,
    ann: &AstAnnotationIndex,
) -> bool {
    if node
        .when
        .iter()
        .any(|w| is_foreign_annotation_constraint(&w.source_module, consuming, ann))
        || node
            .extensions
            .iter()
            .any(|e| is_foreign_annotation_ext(e, consuming, ann))
    {
        return true;
    }
    let musts_foreign = |musts: &[MustExpr]| {
        musts
            .iter()
            .any(|m| is_foreign_annotation_constraint(&m.source_module, consuming, ann))
    };
    let kids_foreign = |kids: &[SchemaNode]| {
        kids.iter()
            .any(|k| node_has_foreign_annotation(k, consuming, ann))
    };
    match &node.kind {
        SchemaNodeKind::Container { children, musts, .. }
        | SchemaNodeKind::Notification { children, musts, .. }
        | SchemaNodeKind::List { children, musts, .. } => {
            musts_foreign(musts) || kids_foreign(children)
        }
        SchemaNodeKind::Leaf { musts, .. }
        | SchemaNodeKind::LeafList { musts, .. }
        | SchemaNodeKind::AnyXml { musts, .. }
        | SchemaNodeKind::AnyData { musts, .. } => musts_foreign(musts),
        SchemaNodeKind::Rpc { input, output, musts, .. } => {
            musts_foreign(musts) || kids_foreign(input) || kids_foreign(output)
        }
        SchemaNodeKind::Action { input, output } => kids_foreign(input) || kids_foreign(output),
        SchemaNodeKind::Case { children } => kids_foreign(children),
        SchemaNodeKind::Choice { cases, .. } => kids_foreign(cases),
        SchemaNodeKind::Uses { .. } => false,
    }
}

/// Clone `nodes`, dropping annotation-injected `must`/`when`/extension statements
/// foreign to `consuming`, recursing into children/cases/input/output.
fn filter_foreign_annotations(
    nodes: &[SchemaNode],
    consuming: &str,
    ann: &AstAnnotationIndex,
) -> Vec<SchemaNode> {
    let retain_musts = |musts: &mut Vec<MustExpr>| {
        musts.retain(|m| !is_foreign_annotation_constraint(&m.source_module, consuming, ann));
    };
    let recurse =
        |kids: &Arc<Vec<SchemaNode>>| Arc::new(filter_foreign_annotations(kids, consuming, ann));
    nodes
        .iter()
        .map(|node| {
            let mut node = node.clone();
            node.when
                .retain(|w| !is_foreign_annotation_constraint(&w.source_module, consuming, ann));
            node.extensions
                .retain(|e| !is_foreign_annotation_ext(e, consuming, ann));
            match &mut node.kind {
                SchemaNodeKind::Container { children, musts, .. }
                | SchemaNodeKind::Notification { children, musts, .. }
                | SchemaNodeKind::List { children, musts, .. } => {
                    retain_musts(musts);
                    *children = recurse(children);
                }
                SchemaNodeKind::Leaf { musts, .. }
                | SchemaNodeKind::LeafList { musts, .. }
                | SchemaNodeKind::AnyXml { musts, .. }
                | SchemaNodeKind::AnyData { musts, .. } => retain_musts(musts),
                SchemaNodeKind::Rpc { input, output, musts, .. } => {
                    retain_musts(musts);
                    *input = recurse(input);
                    *output = recurse(output);
                }
                SchemaNodeKind::Action { input, output } => {
                    *input = recurse(input);
                    *output = recurse(output);
                }
                SchemaNodeKind::Case { children } => *children = recurse(children),
                SchemaNodeKind::Choice { cases, .. } => *cases = recurse(cases),
                SchemaNodeKind::Uses { .. } => {}
            }
            node
        })
        .collect()
}

fn expand_uses_lazy_inner(
    grouping: &Arc<Grouping>,
    source_module_name: Option<&str>,
    uses_overlay: &UsesOverlay,
    own_prefix: &str,
    module_name: &str,
    overlay: &NodeOverlayMap,
    ctx: &ExpansionCtx<'_>,
) -> Arc<Vec<SchemaNode>> {
    let grouping_ptr = Arc::as_ptr(grouping);

    // Fast path: if the cache has a hit AND no overlay mutations are needed,
    // return the Arc directly — zero Vec cloning.
    // The cached Arc was built with the SAME ExpansionCtx (same feature set, same max_status),
    // so it is already correctly feature-filtered for this call.
    // We only need to clone when uses_overlay carries refinements/augments/when/if-feature
    // that must be applied on top of the cached base.
    if let Some(cached_arc) = ctx.cache_get(grouping_ptr, own_prefix) {
        if uses_overlay.is_empty() {
            return cached_arc;
        }
        // Overlay mutations needed: clone and apply them.
        let mut nodes: Vec<SchemaNode> = (*cached_arc).clone();
        let mut ignored_errors = Vec::new();
        let using_module = ctx.registry.resolve_import(module_name, None);
        let prefix_ctx = using_module.as_ref().map(|m| (own_prefix, &m.prefix_map));
        for refine in &uses_overlay.refine_stmts {
            apply_refine_stmt(
                refine,
                &mut nodes,
                module_name,
                prefix_ctx,
                ctx.registry.flags.ignore_unknown_features,
                Some(&ctx.registry.flags),
                &mut ignored_errors,
            );
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
        // If the grouping carries scope_groupings (populated for nested/inline groupings),
        // use those — they include both module-level and sibling groupings visible at the
        // definition site. Otherwise fall back to module-level groupings only.
        let eff_groupings_ref = grouping
            .scope_groupings
            .as_deref()
            .or_else(|| source_module.as_ref().map(|m| m.groupings.as_ref()))
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
            // Attribute `when`/`must` in the grouping body to the module that
            // *defined* the grouping (a submodule, say), not the callsite's
            // prefix-derived module — matching the precompiled path
            // (`grouping_children`) and yanger's per-statement Pos attribution.
            // `source_module_name` is the parent when a submodule grouping is
            // reached via the parent's prefix, so it would mis-attribute.
            &grouping.definer_module_name,
            current_module
                .as_ref()
                .map(|m| m.yang_version)
                .unwrap_or(YangVersion::V11),
            &grouping.def_own_prefix,
            eff_prefix_map_ref,
            ctx.registry,
            eff_groupings_ref,
            &mut ignored_errors,
            // Inline groupings within grouping bodies are not affected by AST-level
            // annotation injection (annotate-module targets module-level groupings).
            &AstAnnotationIndex::default(),
        );
        // Propagate scope_groupings to inner Uses nodes so that transitive
        // nested grouping references can resolve siblings (e.g., vrf-engine-id
        // referencing engine-id-group when both are nested siblings).
        if let Some(ref scope) = grouping.scope_groupings {
            propagate_scope_groupings(&mut nodes, scope);
        }
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
    ctx.cache_insert(grouping_ptr, own_prefix, Arc::clone(&cached_arc), grouping);

    // Now apply the UsesOverlay on top of the cached (immutable) base.
    if uses_overlay.is_empty() && ctx.enabled_features.is_empty() {
        return cached_arc;
    }
    let mut nodes: Vec<SchemaNode> = (*cached_arc).clone();
    let mut ignored_errors = Vec::new();
    let using_module2 = ctx.registry.resolve_import(module_name, None);
    let prefix_ctx2 = using_module2.as_ref().map(|m| (own_prefix, &m.prefix_map));
    for refine in &uses_overlay.refine_stmts {
        apply_refine_stmt(
            refine,
            &mut nodes,
            module_name,
            prefix_ctx2,
            ctx.registry.flags.ignore_unknown_features,
            Some(&ctx.registry.flags),
            &mut ignored_errors,
        );
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
            | SchemaNodeKind::Notification { children: c, .. } => Arc::make_mut(c).extend(nodes),
            SchemaNodeKind::Choice { cases, .. } => {
                // RFC 7950 §7.9.2: non-case nodes in a choice are implicitly wrapped
                // in a case with the same name as the node. Apply wrapping here.
                let cases = Arc::make_mut(cases);
                for node in nodes {
                    match &node.kind {
                        SchemaNodeKind::Case { .. } => cases.push(node),
                        _ => {
                            let case_name = node.name.clone();
                            let case_module = node.module_name.clone();
                            let case_prefix = node.module_prefix.clone();
                            let case_origin = node.origin_module.clone();
                            let case_pos = node.pos.clone();
                            let implicit_case = SchemaNode {
                                name: case_name,
                                module_name: case_module,
                                module_prefix: case_prefix,
                                origin_module: case_origin,
                                pos: case_pos,
                                status: Status::Current,
                                status_pos: None, // synthetic implicit `case` wrapper
                                config: None,
                                when: Vec::new(),
                                if_features: Vec::new(),
                                description: None,
                                reference: None,
                                extensions: Vec::new(),
                                kind: SchemaNodeKind::Case { children: Arc::new(vec![node]) },
                                pmap: HashMap::new(),
                            };
                            cases.push(implicit_case);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

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
            for child in Arc::make_mut(children) {
                fix_module_prefix(child, module_name, new_prefix);
            }
        }
        SchemaNodeKind::Choice { cases, .. } => {
            for case in Arc::make_mut(cases) {
                fix_module_prefix(case, module_name, new_prefix);
            }
        }
        SchemaNodeKind::Rpc { input, output, .. } | SchemaNodeKind::Action { input, output } => {
            for child in Arc::make_mut(input).iter_mut().chain(Arc::make_mut(output).iter_mut()) {
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

fn apply_refine_stmt(
    refine: &Stmt,
    nodes: &mut Vec<SchemaNode>,
    source_module_name: &str,
    prefix_ctx: Option<(&str, &PrefixMap)>,
    ignore_unknown: bool,
    flags: Option<&CompilationFlags>,
    module_errors: &mut Vec<YError>,
) {
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
        source_module_name,
        prefix_ctx,
        true, // refine: `must`/`if-feature` are additive (RFC 7950 §7.13.2)
        ignore_unknown,
        flags,
        module_errors,
    );
}

fn propagate_uses_constraints(
    node: &mut SchemaNode,
    inherited_when: &[WhenExpr],
    inherited_if_features: &[IfFeatureExpr],
) {
    // Per RFC 7950 Section 7.13.2, when a "uses" has a "when" expression it is
    // added only to the TOP-LEVEL schema nodes of the expanded grouping — NOT
    // recursively to their descendants (one level only).
    if !inherited_when.is_empty() {
        // Mark inherited when expressions as non_local — they originated from a `uses`
        // or `augment` statement, so they are evaluated from the parent context and
        // deps must be adjusted by adding a parent step.
        let mut combined: Vec<WhenExpr> = inherited_when
            .iter()
            .map(|w| WhenExpr { non_local: true, ..w.clone() })
            .collect();
        combined.extend(node.when.clone());
        node.when = combined;
    }
    if !inherited_if_features.is_empty() {
        let mut combined = inherited_if_features.to_vec();
        combined.extend(node.if_features.clone());
        node.if_features = combined;
    }

    // Recurse for if_features only (pass empty slice for when).
    // Propagating if_features recursively enables node_visible pruning in
    // expand_children when features are selectively enabled, which is critical
    // for performance with large module sets.  The when constraint is
    // deliberately NOT propagated further (top-level only per RFC 7950).
    match &mut node.kind {
        SchemaNodeKind::Container { children, .. }
        | SchemaNodeKind::List { children, .. }
        | SchemaNodeKind::Case { children }
        | SchemaNodeKind::Notification { children, .. } => {
            for child in Arc::make_mut(children) {
                propagate_uses_constraints(child, &[], inherited_if_features);
            }
        }
        SchemaNodeKind::Choice { cases, .. } => {
            for case in Arc::make_mut(cases) {
                propagate_uses_constraints(case, &[], inherited_if_features);
            }
        }
        SchemaNodeKind::Rpc { input, output, .. } | SchemaNodeKind::Action { input, output } => {
            for child in Arc::make_mut(input) {
                propagate_uses_constraints(child, &[], inherited_if_features);
            }
            for child in Arc::make_mut(output) {
                propagate_uses_constraints(child, &[], inherited_if_features);
            }
        }
        SchemaNodeKind::Leaf { .. }
        | SchemaNodeKind::LeafList { .. }
        | SchemaNodeKind::AnyXml { .. }
        | SchemaNodeKind::AnyData { .. }
        | SchemaNodeKind::Uses { .. } => {}
    }
}

/// Recursively apply augment-level status restriction (RFC 6020 §7.15) to all schema nodes.
/// A `augment { status X; }` defines the status of all added definitions.
fn apply_augment_status(node: &mut SchemaNode, aug_status: Status) {
    if node.status < aug_status {
        node.status = aug_status;
    }
    match &mut node.kind {
        SchemaNodeKind::Container { children, .. }
        | SchemaNodeKind::List { children, .. }
        | SchemaNodeKind::Case { children }
        | SchemaNodeKind::Notification { children, .. } => {
            for child in Arc::make_mut(children) {
                apply_augment_status(child, aug_status);
            }
        }
        SchemaNodeKind::Choice { cases, .. } => {
            for case in Arc::make_mut(cases) {
                apply_augment_status(case, aug_status);
            }
        }
        SchemaNodeKind::Rpc { input, output, .. } | SchemaNodeKind::Action { input, output } => {
            for child in Arc::make_mut(input) {
                apply_augment_status(child, aug_status);
            }
            for child in Arc::make_mut(output) {
                apply_augment_status(child, aug_status);
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
    registry: &ModuleRegistry,
    dev_index: &DeviationIndex,
    module_errors: &mut Vec<YError>,
    overlay: &mut NodeOverlayMap,
) {
    for deviations in dev_index.by_deviating_module.values() {
        for deviation in deviations {
            if !deviation_targets_module(deviation, key) {
                continue;
            }

            // Prefix scope of the deviating module, so a deviate-added `must`/`when`
            // resolves its dependency / override-auto-deps extensions (the deviating
            // module isn't compiled yet at this point — it imports the target — so
            // use the import map captured in the PendingDeviation, not a registry
            // lookup). `own_prefix` is "" because the deviation references the marker
            // extension through an import prefix, never the deviating module's own.
            let dev_prefix_map: PrefixMap = deviation
                .prefix_map
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let dev_prefix_ctx = Some(("", &dev_prefix_map));
            let dev_flags = Some(&registry.flags);

            let Some(target_path) =
                parse_path_internal(&deviation.target_path, true, &deviation.pos, module_errors)
            else {
                continue;
            };

            let target_found = find_node_mut(children, &target_path).is_some();
            let leaf_module = deviation.target_leaf_module_name.clone();
            if !target_found {
                let path = target_path.iter().map(|s| s.name.clone()).collect();
                overlay_entry_mut(overlay, path, leaf_module, |entry| {
                    if entry.source_module.is_none() {
                        entry.source_module = Some(deviation.from_module.name.clone());
                    }
                    entry
                        .deviate_stmts
                        .extend(deviation.deviate_stmts.iter().cloned());
                });
                continue;
            }

            for deviate in &deviation.deviate_stmts {
                match deviate.arg.as_deref() {
                    Some("not-supported") => {
                        if !remove_node_at_path(children, &target_path) {
                            let path = target_path.iter().map(|s| s.name.clone()).collect();
                            overlay_entry_mut(overlay, path, leaf_module.clone(), |entry| {
                                if entry.source_module.is_none() {
                                    entry.source_module = Some(deviation.from_module.name.clone());
                                }
                                entry.deviate_stmts.push(deviate.clone());
                            });
                        }
                    }
                    Some("add") => apply_deviation_edit(
                        children,
                        &target_path,
                        &deviate.substmts,
                        MutationMode::Add,
                        deviation.from_module.name.as_str(),
                        dev_prefix_ctx,
                        dev_flags,
                        &deviation.pos,
                        module_errors,
                    ),
                    Some("replace") => apply_deviation_edit(
                        children,
                        &target_path,
                        &deviate.substmts,
                        MutationMode::Replace,
                        deviation.from_module.name.as_str(),
                        dev_prefix_ctx,
                        dev_flags,
                        &deviation.pos,
                        module_errors,
                    ),
                    Some("delete") => apply_deviation_edit(
                        children,
                        &target_path,
                        &deviate.substmts,
                        MutationMode::Delete,
                        deviation.from_module.name.as_str(),
                        dev_prefix_ctx,
                        dev_flags,
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
    augments: &mut Vec<AugmentEntry>,
    ann_index: &AnnotationIndex,
    module_errors: &mut Vec<YError>,
    overlay: &mut NodeOverlayMap,
    grouping_children: &HashMap<String, Arc<Vec<SchemaNode>>>,
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
            apply_annotation_to_node(node, ann);
        } else if let Some(node) = find_node_in_augments(augments, &target_path, grouping_children) {
            apply_annotation_to_node(node, ann);
        } else {
            // Target is inside an unexpanded `uses` — defer to overlay.
            let path = target_path.iter().map(|s| s.name.clone()).collect();
            // The leaf module disambiguates same-named siblings from different
            // modules (e.g. two augments under the same target).
            let leaf_module = target_path
                .last()
                .and_then(|s| s.prefix.as_ref())
                .and_then(|p| ann.prefix_map.get(p))
                .cloned();
            overlay_entry_mut(overlay, path, leaf_module, |entry| {
                entry.annotations.push(crate::compiler::types::Annotation {
                    instances: ann.instances.clone(),
                    when_stmts: ann.when_stmts.clone(),
                    must_stmts: ann.must_stmts.clone(),
                    source_module: ann.from_module.name.clone(),
                    source_revision: ann.from_module.revision.clone(),
                    source_plugin: ann.source_plugin,
                });
            });
        }
    }
}

/// Apply annotation (extensions, whens, musts) to a resolved target node.
fn apply_annotation_to_node(node: &mut SchemaNode, ann: &PendingAnnotation) {
    node.extensions.extend(ann.instances.iter().map(|inst| {
        let mut inst = inst.clone();
        inst.injection_source_module = Some(ann.from_module.name.clone());
        inst
    }));
    let source = ann.from_module.name.as_str();
    let source_rev = ann.from_module.revision.clone();
    for when in &ann.when_stmts {
        mutate_when_exprs(&mut node.when, when, MutationMode::Add, source, source_rev.clone(), None, None);
    }
    match &mut node.kind {
        SchemaNodeKind::Container { musts, .. }
        | SchemaNodeKind::Leaf { musts, .. }
        | SchemaNodeKind::LeafList { musts, .. }
        | SchemaNodeKind::List { musts, .. }
        | SchemaNodeKind::Rpc { musts, .. }
        | SchemaNodeKind::Notification { musts, .. }
        | SchemaNodeKind::AnyXml { musts, .. }
        | SchemaNodeKind::AnyData { musts, .. } => {
            for must in &ann.must_stmts {
                mutate_must_exprs(musts, must, MutationMode::Add, source, source_rev.clone(), None, None);
            }
        }
        _ => {}
    }
}

/// Search for a target node inside external augment bodies.
/// The annotation path may start with the augment's target_path prefix; the
/// remaining steps navigate into the augment body.
/// When the body contains unexpanded `Uses` nodes, navigates through the grouping's
/// pre-compiled children to locate the target, then stores the annotation in the
/// Uses node's overlay for application during normal expansion.
fn find_node_in_augments<'a>(
    augments: &'a mut Vec<AugmentEntry>,
    target_path: &[PathStep],
    grouping_children: &HashMap<String, Arc<Vec<SchemaNode>>>,
) -> Option<&'a mut SchemaNode> {
    for aug in augments.iter_mut() {
        let aug_len = aug.target_path.len();
        if target_path.len() <= aug_len {
            continue;
        }
        // Check if the annotation path starts with this augment's target_path
        // (compare by name only — prefixes may differ between the annotation
        // module and the augmenting module).
        let matches = aug.target_path.iter().zip(target_path.iter()).all(|(a, b)| a.name == b.name);
        if !matches {
            continue;
        }
        // Remaining path navigates into the augment body.
        let remaining = &target_path[aug_len..];
        // Try direct navigation first (handles pre-expanded bodies or non-Uses nodes).
        if find_node_mut(&mut aug.nodes, remaining).is_some() {
            return find_node_mut(&mut aug.nodes, remaining);
        }
        // If the body contains Uses nodes, check if the target exists inside
        // any grouping body. If found, we don't expand here — we'll return None
        // and let the caller defer to the overlay mechanism with the FULL path
        // (not just the remaining part), which correctly identifies the target
        // during Uses expansion.
        // This avoids corrupting the augment body structure.
    }
    None
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
/// the same self-declared prefix letter (e.g. a family of submodules that all
/// use `prefix ex`).
/// Returns `true` if `deviation` should be applied when compiling module `key`.
///
/// Routing is based on the **last** path step's prefix (the module that owns
/// the leaf node), which matches how `expand_children` routes overlay entries
/// for augmented nodes.  For non-augmented paths (all steps share the same
/// prefix) this is identical to the old first-prefix routing.
fn deviation_targets_module(deviation: &PendingDeviation, key: &ModuleKey) -> bool {
    // Primary check: use the leaf module (last path step's prefix).
    // This correctly routes cross-module augment deviations like
    // `/A:foo/B:bar` to module B (where B augmented `bar` into A).
    match &deviation.target_leaf_module_name {
        Some(module_name) => {
            if module_name == &key.name {
                return true;
            }
            // Leaf module doesn't match. Fall back to root module check for
            // deviations where first and last module differ (e.g., the leaf
            // is from yet another module that the root module imports).
            // We also accept the root module as a target so that `find_node_mut`
            // can store the deviation in the overlay for later augment expansion.
            match &deviation.target_module_name {
                Some(root_module) => root_module == &key.name,
                None => match &deviation.target_prefix {
                    None => deviation.from_module.name == key.name,
                    Some(_) => false,
                },
            }
        }
        // No leaf module name: either the last step has no prefix (self-deviation)
        // or the prefix wasn't found in the imports (malformed file).
        None => match &deviation.target_leaf_prefix {
            // No prefix on the leaf step → fall back to root module routing.
            None => match &deviation.target_module_name {
                Some(module_name) => module_name == &key.name,
                None => match &deviation.target_prefix {
                    None => deviation.from_module.name == key.name,
                    Some(_) => false,
                },
            },
            // Prefix present but unresolved → cannot determine target; skip.
            Some(_) => false,
        },
    }
}

fn apply_deviation_edit(
    nodes: &mut Vec<SchemaNode>,
    target_path: &[PathStep],
    stmts: &[Stmt],
    mode: MutationMode,
    source_module_name: &str,
    prefix_ctx: Option<(&str, &PrefixMap)>,
    flags: Option<&CompilationFlags>,
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

    apply_node_mutation(
        target,
        stmts,
        mode,
        source_module_name,
        prefix_ctx,
        false, // deviation, not refine
        false, // if-feature parsing is refine-only; unused for deviations
        flags,
        module_errors,
    );
}

fn apply_node_mutation(
    node: &mut SchemaNode,
    stmts: &[Stmt],
    mode: MutationMode,
    source_module_name: &str,
    prefix_ctx: Option<(&str, &PrefixMap)>,
    // True when the mutation comes from a `refine` (vs a `deviation`). Per RFC
    // 7950 §7.13.2, a refine's `must` and `if-feature` are *added* to the
    // target's existing lists, not replaced — unlike `deviate replace`.
    is_refine: bool,
    // Whether unknown features/prefixes in a refine's `if-feature` are tolerated
    // (treated as disabled) rather than reported. Only consulted on the refine
    // path; deviations never parse `if-feature` here.
    ignore_unknown: bool,
    // Compilation flags, used (with `prefix_ctx`) to resolve dependency /
    // override-auto-deps extensions inside a `must`/`when` body. `None` when the
    // caller has no flag context.
    flags: Option<&CompilationFlags>,
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
                MutationMode::Delete => {
                    node.status = Status::Current;
                    node.status_pos = None;
                }
                MutationMode::Add if node.status != Status::Current => {}
                _ => {
                    node.status = parse_status_arg(stmt, module_errors);
                    // Re-point to the refine/deviate `status` substatement.
                    node.status_pos = Some(stmt.pos.clone());
                }
            },
            Some(BuiltInKeyword::Description) => {
                apply_opt_string(mode, &mut node.description, stmt.arg.clone())
            }
            Some(BuiltInKeyword::Reference) => {
                apply_opt_string(mode, &mut node.reference, stmt.arg.clone())
            }
            Some(BuiltInKeyword::When) => {
                mutate_when_exprs(&mut node.when, stmt, mode, source_module_name, None, prefix_ctx, flags)
            }
            Some(BuiltInKeyword::IfFeature) => {
                if is_refine {
                    // RFC 7950 §7.13.2: a refine adds `if-feature` expressions to
                    // the target's existing list (like `must`). Parse in the using
                    // module's context (its prefix/imports), which `prefix_ctx`
                    // carries.
                    if let (Some((own_prefix, prefix_map)), Some(raw)) =
                        (prefix_ctx, stmt.arg.as_deref())
                    {
                        if let Some(expr) = parse_if_feature_expr(
                            raw,
                            own_prefix,
                            source_module_name,
                            prefix_map,
                            &stmt.pos,
                            module_errors,
                            ignore_unknown,
                        ) {
                            node.if_features.push(expr);
                        }
                    }
                } else if mode == MutationMode::Delete {
                    node.if_features.clear();
                }
            }
            Some(BuiltInKeyword::Must) => {
                // RFC 7950 §7.13.2: a refine adds `must` expressions to the
                // existing list; only a deviation replaces them.
                let must_mode = if is_refine { MutationMode::Add } else { mode };
                match &mut node.kind {
                    SchemaNodeKind::Container { musts, .. }
                    | SchemaNodeKind::Leaf { musts, .. }
                    | SchemaNodeKind::LeafList { musts, .. }
                    | SchemaNodeKind::List { musts, .. }
                    | SchemaNodeKind::Rpc { musts, .. }
                    | SchemaNodeKind::Notification { musts, .. }
                    | SchemaNodeKind::AnyXml { musts, .. }
                    | SchemaNodeKind::AnyData { musts, .. } => {
                        mutate_must_exprs(musts, stmt, must_mode, source_module_name, None, prefix_ctx, flags)
                    }
                    _ => {}
                }
            }
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
            _ => {
                // Handle extension statements (e.g. a plugin extension in a refine).
                if let Some((own_prefix, prefix_map)) = prefix_ctx {
                    if let Some((module, name)) = resolve_kw(&stmt.keyword, own_prefix, source_module_name, prefix_map) {
                        let ext = ExtensionInstance {
                            module: module.to_string(),
                            name: name.to_string(),
                            arg: stmt.arg.clone(),
                            substmts: stmt.substmts.clone(),
                            pos: stmt.pos.clone(),
                            injection_source_module: None,
                        };
                        match mode {
                            MutationMode::Delete => {
                                node.extensions.retain(|e| !(e.name == name && e.module == module));
                            }
                            _ => {
                                node.extensions.push(ext);
                            }
                        }
                    }
                }
            }
        }
    }
}

fn mutate_when_exprs(
    list: &mut Vec<WhenExpr>,
    stmt: &Stmt,
    mode: MutationMode,
    source_module_name: &str,
    source_revision: Option<String>,
    // Prefix scope + flags of the module that authored `stmt` (a refine's using
    // module, or a deviation's deviating module). When both are present, the
    // configured dependency / override-auto-deps extensions in the statement
    // body are resolved into the expression — exactly as the direct compile path
    // does. Annotation-injected whens have no prefix context and pass `None`.
    prefix_ctx: Option<(&str, &PrefixMap)>,
    flags: Option<&CompilationFlags>,
) {
    let (explicit_deps, override_auto_deps) = match (prefix_ctx, flags) {
        (Some((own_prefix, prefix_map)), Some(flags)) => {
            collect_explicit_deps(&stmt.substmts, own_prefix, source_module_name, prefix_map, flags)
        }
        _ => (Vec::new(), false),
    };
    let entry = WhenExpr {
        xpath: stmt.arg.clone().unwrap_or_default(),
        description: opt_substmt_arg(stmt, BuiltInKeyword::Description),
        reference: opt_substmt_arg(stmt, BuiltInKeyword::Reference),
        source_module: source_module_name.to_string(),
        source_revision,
        non_local: false,
        explicit_deps,
        override_auto_deps,
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

fn mutate_must_exprs(
    list: &mut Vec<MustExpr>,
    stmt: &Stmt,
    mode: MutationMode,
    source_module_name: &str,
    source_revision: Option<String>,
    // See [`mutate_when_exprs`]: prefix scope + flags of the module that authored
    // `stmt`, used to resolve dependency / override-auto-deps extensions in its
    // body. `None` for annotation-injected musts (no prefix context).
    prefix_ctx: Option<(&str, &PrefixMap)>,
    flags: Option<&CompilationFlags>,
) {
    let (explicit_deps, override_auto_deps) = match (prefix_ctx, flags) {
        (Some((own_prefix, prefix_map)), Some(flags)) => {
            collect_explicit_deps(&stmt.substmts, own_prefix, source_module_name, prefix_map, flags)
        }
        _ => (Vec::new(), false),
    };
    let entry = MustExpr {
        xpath: stmt.arg.clone().unwrap_or_default(),
        error_message: opt_substmt_arg(stmt, BuiltInKeyword::ErrorMessage),
        error_app_tag: opt_substmt_arg(stmt, BuiltInKeyword::ErrorAppTag),
        description: opt_substmt_arg(stmt, BuiltInKeyword::Description),
        source_module: source_module_name.to_string(),
        source_revision,
        explicit_deps,
        override_auto_deps,
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
        | SchemaNodeKind::Notification { children, .. } => find_node_mut(Arc::make_mut(children), path),
        SchemaNodeKind::Choice { cases, .. } => find_node_mut(Arc::make_mut(cases), path),
        SchemaNodeKind::Rpc { input, output, .. } | SchemaNodeKind::Action { input, output } => {
            if head.name == "input" {
                if tail.is_empty() {
                    None
                } else {
                    find_node_mut(Arc::make_mut(input), tail)
                }
            } else if head.name == "output" {
                if tail.is_empty() {
                    None
                } else {
                    find_node_mut(Arc::make_mut(output), tail)
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
            | SchemaNodeKind::Notification { children, .. } => remove_node_at_path(Arc::make_mut(children), tail),
            SchemaNodeKind::Choice { cases, .. } => remove_node_at_path(Arc::make_mut(cases), tail),
            SchemaNodeKind::Rpc { input, output, .. }
            | SchemaNodeKind::Action { input, output } => {
                if tail[0].name == "input" {
                    if tail.len() > 1 {
                        remove_node_at_path(Arc::make_mut(input), &tail[1..])
                    } else {
                        false
                    }
                } else if tail[0].name == "output" {
                    if tail.len() > 1 {
                        remove_node_at_path(Arc::make_mut(output), &tail[1..])
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

/// Collect the deviation modules that were applied to `key` during this compilation.
///
/// Returns `true` if any deviate statement in `deviations` adds or replaces a
/// `must` or `when` statement.
fn deviations_have_must_or_when(deviations: &[PendingDeviation]) -> bool {
    deviations.iter().any(|d| {
        d.deviate_stmts.iter().any(|deviate| {
            let arg = deviate.arg.as_deref().unwrap_or("");
            if arg != "add" && arg != "replace" {
                return false;
            }
            deviate.substmts.iter().any(|sub| {
                matches!(
                    &sub.keyword,
                    Keyword::BuiltIn(k)
                        if *k == BuiltInKeyword::Must || *k == BuiltInKeyword::When
                )
            })
        })
    })
}

/// Returns an [`AppliedDeviations`] value ready to be stored with
/// [`CompiledModule::set_pdata`].
fn collect_applied_deviations(key: &ModuleKey, dev_index: &DeviationIndex) -> AppliedDeviations {
    let mut result: Vec<(String, Option<String>, PrefixMap, bool)> = dev_index
        .by_deviating_module
        .iter()
        .filter_map(|(dev_key, deviations)| {
            let targets_us = deviations.iter().any(|d| {
                if deviation_targets_module(d, key) {
                    return true;
                }
                // Also check the last prefix-qualified step: a deviation path like
                // `/a:root/b:sub` targets `b`, not `a`.
                let mut ignored = Vec::new();
                if let Some(path) = parse_path_internal(&d.target_path, true, &d.pos, &mut ignored)
                {
                    if let Some(last) = path.last() {
                        if let Some(ref pfx) = last.prefix {
                            if let Some(mod_name) = d.prefix_map.get(pfx) {
                                if mod_name == &key.name {
                                    return true;
                                }
                            }
                        }
                    }
                }
                false
            });
            if targets_us {
                let prefix_map: PrefixMap = deviations.first()
                    .map(|d| d.prefix_map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default();
                let has_must_or_when = deviations_have_must_or_when(deviations);
                Some((dev_key.name.clone(), dev_key.revision.clone(), prefix_map, has_must_or_when))
            } else {
                None
            }
        })
        .collect();
    result.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    AppliedDeviations(result)
}

/// Collect the annotation modules that were applied to `key` during this compilation.
///
/// Returns an [`AppliedAnnotations`] value ready to be stored with
/// [`CompiledModule::set_pdata`].
fn collect_applied_annotations(
    key: &ModuleKey,
    ann_index: &AnnotationIndex,
) -> AppliedAnnotations {
    // Group annotations by source module, tracking prefix_map, has_when_or_must, root_is_self,
    // and the set of YANG prefixes used in extension instance arguments.
    let mut seen: std::collections::HashMap<
        String,
        (Option<String>, PrefixMap, bool, bool, std::collections::HashSet<String>),
    > = std::collections::HashMap::new();
    if let Some(pending) = ann_index.by_target_module.get(&key.name) {
        for ann in pending {
            let has_wm = !ann.when_stmts.is_empty() || !ann.must_stmts.is_empty();
            // Collect prefixes used in extension instance arguments (e.g. dependency paths).
            let inst_prefixes: std::collections::HashSet<String> = ann
                .instances
                .iter()
                .filter_map(|inst| inst.arg.as_deref())
                .flat_map(extract_yang_path_prefixes)
                .collect();
            // Determine if this annotation's paths root directly into the target module.
            let root_is_self = annotation_root_is_target(&ann.target_path, &ann.prefix_map, &key.name);
            seen.entry(ann.from_module.name.clone())
                .and_modify(|e| {
                    e.2 |= has_wm;
                    e.3 |= root_is_self;
                    e.4.extend(inst_prefixes.iter().cloned());
                })
                .or_insert_with(|| {
                    let prefix_map: PrefixMap = ann
                        .prefix_map
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    (ann.from_module.revision.clone(), prefix_map, has_wm, root_is_self, inst_prefixes)
                });
        }
    }
    let mut result: Vec<(String, Option<String>, PrefixMap, bool, bool, Vec<String>)> = seen
        .into_iter()
        .map(|(name, (rev, pm, hwm, ris, ep))| {
            let mut ext_prefixes: Vec<String> = ep.into_iter().collect();
            ext_prefixes.sort_unstable();
            (name, rev, pm, hwm, ris, ext_prefixes)
        })
        .collect();
    result.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    AppliedAnnotations(result)
}

/// Extract YANG identifier prefixes from a path-like string argument.
///
/// Looks for patterns like `pfx:localname` and returns the `pfx` parts.
/// Skips URL-like constructs (e.g. `http://`) to avoid false matches.
fn extract_yang_path_prefixes(s: &str) -> Vec<String> {
    let mut prefixes = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip to start of an identifier-like token
        while i < bytes.len()
            && !(bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b'_')
        {
            i += 1;
        }
        let start = i;
        // Scan the token: allow alphanumeric, hyphen, underscore, dot
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric()
                || bytes[i] == b'-'
                || bytes[i] == b'_'
                || bytes[i] == b'.')
        {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b':' && i > start {
            let next = if i + 1 < bytes.len() { bytes[i + 1] } else { 0 };
            // Skip URLs like http:// or urn: followed by /
            if next != b'/' {
                let prefix = &s[start..i];
                prefixes.push(prefix.to_string());
            }
            i += 1; // skip the ':'
        } else {
            i += 1; // skip past current position
        }
    }
    prefixes.sort_unstable();
    prefixes.dedup();
    prefixes
}

/// Returns true if the annotation's `target_path` starts with a prefix that resolves
/// to `target_module_name` via the annotation module's `prefix_map`.
///
/// Example: path `/ex:root`, prefix_map has `ex → ex`
/// → first prefix = `ex`, resolves to module `ex` → `root_is_self = true`.
fn annotation_root_is_target(
    target_path: &str,
    ann_prefix_map: &HashMap<String, String>,
    target_module_name: &str,
) -> bool {
    // Strip leading '/'; find the first colon-prefixed step.
    let path = target_path.trim_start_matches('/');
    let colon_pos = match path.find(':') {
        Some(p) => p,
        None => return false, // no prefix → can't determine
    };
    // Verify the prefix is before any '/' separator.
    let slash_pos = path.find('/').unwrap_or(path.len());
    if colon_pos > slash_pos {
        return false;
    }
    let first_prefix = &path[..colon_pos];
    // Resolve prefix → module name using the annotation module's prefix_map.
    ann_prefix_map
        .get(first_prefix)
        .map(|mod_name| mod_name == target_module_name)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::astannindex::AstAnnotationIndex;
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
            &AstAnnotationIndex::default(),
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

    /// Find the resolved `must`s on leaf `c/x` after expanding `c`'s children.
    #[cfg(test)]
    fn musts_on_c_x<'a>(
        module: &'a CompiledModule,
        ctx: &ExpansionCtx<'_>,
    ) -> Vec<(String, Vec<String>, bool)> {
        let c = module
            .children
            .iter()
            .find(|n| n.name == "c")
            .expect("container c");
        let kids = c.children(ctx);
        let x = kids.iter().find(|n| n.name == "x").expect("leaf x");
        match &x.kind {
            SchemaNodeKind::Leaf { musts, .. } => musts
                .iter()
                .map(|m| (m.xpath.clone(), m.explicit_deps.clone(), m.override_auto_deps))
                .collect(),
            other => panic!("expected leaf, got {other:?}"),
        }
    }

    #[test]
    fn refine_must_preserves_explicit_deps() {
        // A `dependency` extension marks an explicit dependency path inside a must.
        // When that must is introduced through a `refine`, the resolved MustExpr
        // must still carry explicit_deps (the same way a direct must does) — the
        // mutate path used to drop it.
        let deps_ext = parse_module(
            r#"
module example-deps {
  namespace "urn:example:deps";
  prefix ed;
  extension dependency { argument path; }
}
"#,
        );
        let model = parse_module(
            r#"
module example-bug {
  namespace "urn:example:bug";
  prefix eb;
  import example-deps { prefix ed; }
  grouping g {
    leaf x { type string; }
  }
  container c {
    uses g {
      refine "x" {
        must "../sibling" {
          ed:dependency "../sibling";
        }
      }
    }
    leaf sibling { type string; }
  }
}
"#,
        );

        let mut reg = ModuleRegistry::new();
        reg.flags.dependency_extension = Some(("example-deps".into(), "dependency".into()));
        let deps_compiled = compile_module(
            &ModuleKey::latest("example-deps"),
            deps_ext,
            &reg,
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
            &AstAnnotationIndex::default(),
        );
        reg.insert(Arc::new(deps_compiled));
        let model_compiled = compile_module(
            &ModuleKey::latest("example-bug"),
            model,
            &reg,
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
            &AstAnnotationIndex::default(),
        );
        assert!(model_compiled.errors.is_empty(), "errors: {:?}", model_compiled.errors);
        reg.insert(Arc::new(model_compiled));

        let module = reg.resolve_import("example-bug", None).unwrap();
        let ctx = ExpansionCtx::all_features(&reg);
        let musts = musts_on_c_x(&module, &ctx);
        assert_eq!(
            musts,
            vec![("../sibling".to_string(), vec!["../sibling".to_string()], false)],
            "refine-introduced must must keep its explicit_deps"
        );
    }

    #[test]
    fn deviation_must_preserves_override_auto_deps() {
        // The override-auto-deps marker on a deviate-added must must survive the
        // mutate path (deviating module's prefix scope is used to resolve it).
        let deps_ext = parse_module(
            r#"
module example-deps {
  namespace "urn:example:deps";
  prefix ed;
  extension no-auto-deps { }
}
"#,
        );
        let base = parse_module(
            r#"
module example-base {
  namespace "urn:example:base";
  prefix eb;
  container c {
    leaf x { type string; }
    leaf sibling { type string; }
  }
}
"#,
        );
        let dev = parse_module(
            r#"
module example-dev {
  namespace "urn:example:dev";
  prefix ev;
  import example-base { prefix eb; }
  import example-deps { prefix ed; }
  deviation "/eb:c/eb:x" {
    deviate add {
      must "../sibling" {
        ed:no-auto-deps;
      }
    }
  }
}
"#,
        );

        let dev_index = DeviationIndex::build(&[
            (ModuleKey::latest("example-dev"), dev.clone()),
        ]);
        let mut reg = ModuleRegistry::new();
        reg.flags.override_auto_deps_extension =
            Some(("example-deps".into(), "no-auto-deps".into()));
        for (name, stmt) in [("example-deps", deps_ext), ("example-base", base)] {
            let compiled = compile_module(
                &ModuleKey::latest(name),
                stmt,
                &reg,
                &dev_index,
                &AnnotationIndex::default(),
                &AstAnnotationIndex::default(),
            );
            reg.insert(Arc::new(compiled));
        }

        let module = reg.resolve_import("example-base", None).unwrap();
        let ctx = ExpansionCtx::all_features(&reg);
        let musts = musts_on_c_x(&module, &ctx);
        assert_eq!(
            musts,
            vec![("../sibling".to_string(), Vec::<String>::new(), true)],
            "deviate-added must must keep override_auto_deps set"
        );
    }

    #[test]
    fn deferred_deviation_must_preserves_explicit_deps() {
        // Deviation target is a node introduced via `uses`, so it does not exist in
        // the raw tree at compile time and is deferred to the node overlay; the
        // must is materialised later by `apply_overlay_entry` at expansion. Its
        // explicit_deps must survive that path too (the deviating module's prefix
        // scope is resolved from the registry there).
        let deps_ext = parse_module(
            r#"
module example-deps {
  namespace "urn:example:deps";
  prefix ed;
  extension dependency { argument path; }
}
"#,
        );
        let base = parse_module(
            r#"
module example-base {
  namespace "urn:example:base";
  prefix eb;
  grouping g {
    leaf x { type string; }
  }
  container c {
    uses g;
    leaf sibling { type string; }
  }
}
"#,
        );
        let dev = parse_module(
            r#"
module example-dev {
  namespace "urn:example:dev";
  prefix ev;
  import example-base { prefix eb; }
  import example-deps { prefix ed; }
  deviation "/eb:c/eb:x" {
    deviate add {
      must "../sibling" {
        ed:dependency "../sibling";
      }
    }
  }
}
"#,
        );

        let dev_index = DeviationIndex::build(&[(ModuleKey::latest("example-dev"), dev.clone())]);
        let mut reg = ModuleRegistry::new();
        reg.flags.dependency_extension = Some(("example-deps".into(), "dependency".into()));
        // The deviating module must also be compiled into the registry: the
        // deferred-deviation path resolves its prefix scope from there at expansion.
        for (name, stmt) in [
            ("example-deps", deps_ext),
            ("example-base", base),
            ("example-dev", dev),
        ] {
            let compiled = compile_module(
                &ModuleKey::latest(name),
                stmt,
                &reg,
                &dev_index,
                &AnnotationIndex::default(),
                &AstAnnotationIndex::default(),
            );
            reg.insert(Arc::new(compiled));
        }

        let module = reg.resolve_import("example-base", None).unwrap();
        let ctx = ExpansionCtx::all_features(&reg);
        let musts = musts_on_c_x(&module, &ctx);
        assert_eq!(
            musts,
            vec![("../sibling".to_string(), vec!["../sibling".to_string()], false)],
            "deviate-added must on a uses-introduced node must keep its explicit_deps"
        );
    }

    #[test]
    fn status_pos_records_declaration_position_for_ordering() {
        // status_pos lets a backend order `status` against neighbouring
        // substatements (here an extension) by source position. The two lists
        // declare the pair in opposite order; the relative line of status_pos vs
        // the extension's pos must reflect that.
        let ext_mod = parse_module(
            r#"
module acme-ext {
  namespace "urn:acme:ext";
  prefix acme;
  extension alt-name { argument name; }
}
"#,
        );
        let stmt = parse_module(
            r#"
module example-status-pos {
  yang-version 1.1;
  namespace "urn:example:status-pos";
  prefix esp;
  import acme-ext { prefix acme; }

  // alt-name declared BEFORE status
  list a {
    key id;
    acme:alt-name "a-old";
    status obsolete;
    leaf id { type string; }
  }
  // status declared BEFORE alt-name
  list b {
    key id;
    status obsolete;
    acme:alt-name "b-old";
    leaf id { type string; }
  }
  // no explicit status
  list c {
    key id;
    leaf id { type string; }
  }
}
"#,
        );
        let mut reg = ModuleRegistry::new();
        reg.insert(Arc::new(compile_module(
            &ModuleKey::latest("acme-ext"),
            ext_mod,
            &reg,
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
            &AstAnnotationIndex::default(),
        )));
        let compiled = compile_module(
            &ModuleKey::latest("example-status-pos"),
            stmt,
            &reg,
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
            &AstAnnotationIndex::default(),
        );
        assert!(compiled.errors.is_empty(), "errors: {:?}", compiled.errors);

        let line = |p: &Pos| match p {
            Pos::FilePos { line, .. } => *line,
            other => panic!("expected FilePos, got {other:?}"),
        };
        let get = |name: &str| {
            compiled
                .children
                .iter()
                .find(|n| n.name == name)
                .unwrap_or_else(|| panic!("node {name}"))
        };
        let alt_name_pos = |n: &SchemaNode| {
            n.extensions
                .iter()
                .find(|e| e.name == "alt-name")
                .unwrap_or_else(|| panic!("alt-name on {}", n.name))
                .pos
                .clone()
        };

        let a = get("a");
        let a_status = a.status_pos.as_ref().expect("a has explicit status");
        assert!(
            line(a_status) > line(&alt_name_pos(a)),
            "in `a`, status is declared after alt-name"
        );

        let b = get("b");
        let b_status = b.status_pos.as_ref().expect("b has explicit status");
        assert!(
            line(b_status) < line(&alt_name_pos(b)),
            "in `b`, status is declared before alt-name"
        );

        assert_eq!(
            get("c").status_pos,
            None,
            "implicit `current` status carries no position"
        );
    }

    #[test]
    fn walker_sees_deviation_through_uses() {
        // A deviation targets a node reached through a `uses`, so it can't be
        // applied at compile time and is deferred to the module overlay. The
        // no-clone walkers (used e.g. for leafref-target resolution) must observe
        // the deviated node, not the raw grouping body — i.e. the same tree the
        // main expansion path produces.
        let target = parse_module(
            r#"
module target {
  namespace "urn:example:target";
  prefix t;
  grouping g {
    list item {
      key id;
      leaf id { type uint32 { range "1..512"; } }
    }
  }
  container root { uses g; }
}
"#,
        );
        let dev = parse_module(
            r#"
module target-deviation {
  namespace "urn:example:target-deviation";
  prefix td;
  import target { prefix t; }
  deviation "/t:root/t:item/t:id" {
    deviate replace {
      type uint32 { range "1..64"; }
    }
  }
}
"#,
        );

        let dev_index =
            DeviationIndex::build(&[(ModuleKey::latest("target-deviation"), dev.clone())]);
        let mut reg = ModuleRegistry::new();
        // The deviating module must be in the registry: the deferred-overlay owner
        // is resolved from there, mirroring a real bundle.
        for (name, stmt) in [("target", target), ("target-deviation", dev)] {
            reg.insert(Arc::new(compile_module(
                &ModuleKey::latest(name),
                stmt,
                &reg,
                &dev_index,
                &AnnotationIndex::default(),
                &AstAnnotationIndex::default(),
            )));
        }

        let module = reg.resolve_import("target", None).unwrap();
        let ctx = ExpansionCtx::all_features(&reg);

        let path = |s: &str| -> Vec<PathStep> {
            s.split('/')
                .filter(|p| !p.is_empty())
                .map(|name| PathStep { prefix: None, name: name.to_string() })
                .collect()
        };

        let range_of = |n: &SchemaNode| match &n.kind {
            SchemaNodeKind::Leaf { type_stmt, .. } => type_stmt
                .get_substmt(BuiltInKeyword::Range)
                .and_then(|s| s.arg.clone())
                .expect("range present"),
            other => panic!("expected leaf, got {other:?}"),
        };

        // 1) The walker sees the deviated range.
        let (terminal, _) =
            walk_path_collecting_intermediates(&path("root/item/id"), &module.children, &ctx)
                .expect("path resolves");
        assert_eq!(
            range_of(&terminal),
            "1..64",
            "the walker must see the deviated range, not the original 1..512"
        );

        // 2) Parity: the main expansion path produces the same deviated range.
        let root = module.children.iter().find(|n| n.name == "root").unwrap();
        let item = root.children(&ctx).into_iter().find(|n| n.name == "item").unwrap();
        let id = item.children(&ctx).into_iter().find(|n| n.name == "id").unwrap();
        assert_eq!(range_of(&id), "1..64", "main path also deviated (sanity)");
    }

    #[test]
    fn walker_sees_deviation_through_nested_uses() {
        // Like `walker_sees_deviation_through_uses`, but the descent crosses a
        // `uses` at an *intermediate* level (inside a grouping body reached via an
        // outer `uses`). The `_` arm descends via `children()` (overlay-correct),
        // but the residual `Uses` arm expanded with an empty overlay — so a
        // deferred deviation under the nested `uses` was lost only on the walker's
        // path, while the main expansion path applied it.
        let target = parse_module(
            r#"
module target {
  namespace "urn:example:target";
  prefix t;
  grouping inner {
    list item {
      key id;
      leaf id { type uint32 { range "1..512"; } }
    }
  }
  grouping outer {
    container holder { uses inner; }
  }
  container root { uses outer; }
}
"#,
        );
        let dev = parse_module(
            r#"
module target-deviation {
  namespace "urn:example:target-deviation";
  prefix td;
  import target { prefix t; }
  deviation "/t:root/t:holder/t:item/t:id" {
    deviate replace {
      type uint32 { range "1..64"; }
    }
  }
}
"#,
        );

        let dev_index =
            DeviationIndex::build(&[(ModuleKey::latest("target-deviation"), dev.clone())]);
        let mut reg = ModuleRegistry::new();
        for (name, stmt) in [("target", target), ("target-deviation", dev)] {
            reg.insert(Arc::new(compile_module(
                &ModuleKey::latest(name),
                stmt,
                &reg,
                &dev_index,
                &AnnotationIndex::default(),
                &AstAnnotationIndex::default(),
            )));
        }

        let module = reg.resolve_import("target", None).unwrap();
        let ctx = ExpansionCtx::all_features(&reg);

        let path = |s: &str| -> Vec<PathStep> {
            s.split('/')
                .filter(|p| !p.is_empty())
                .map(|name| PathStep { prefix: None, name: name.to_string() })
                .collect()
        };
        let range_of = |n: &SchemaNode| match &n.kind {
            SchemaNodeKind::Leaf { type_stmt, .. } => type_stmt
                .get_substmt(BuiltInKeyword::Range)
                .and_then(|s| s.arg.clone())
                .expect("range present"),
            other => panic!("expected leaf, got {other:?}"),
        };

        // Main path applies the deviation (sanity).
        let root = module.children.iter().find(|n| n.name == "root").unwrap();
        let holder = root.children(&ctx).into_iter().find(|n| n.name == "holder").unwrap();
        let item = holder.children(&ctx).into_iter().find(|n| n.name == "item").unwrap();
        let id = item.children(&ctx).into_iter().find(|n| n.name == "id").unwrap();
        assert_eq!(range_of(&id), "1..64", "main path deviated (sanity)");

        // The walker must agree, even crossing the nested `uses`.
        let (terminal, _) =
            walk_path_collecting_intermediates(&path("root/holder/item/id"), &module.children, &ctx)
                .expect("path resolves");
        assert_eq!(
            range_of(&terminal),
            "1..64",
            "the walker must see the deviated range across a nested uses, not 1..512"
        );
    }

    #[test]
    fn walker_sees_deviation_through_cross_module_uses() {
        // The real-world shape: the grouping lives in a *different* module from the
        // one that `uses` it and carries the deviation. Grouping-expanded nodes keep
        // the grouping host's `module_name`, so `SchemaNode::children` resolves the
        // host's (empty) overlay rather than the consuming module's — where the
        // deferred deviation actually lives.
        let common = parse_module(
            r#"
module common {
  namespace "urn:common"; prefix c;
  grouping g {
    container holder {
      list item {
        key id;
        leaf id { type uint32 { range "1..512"; } }
      }
    }
  }
}
"#,
        );
        let native = parse_module(
            r#"
module native {
  namespace "urn:native"; prefix n;
  import common { prefix c; }
  container root { uses c:g; }
}
"#,
        );
        let dev = parse_module(
            r#"
module native-deviation {
  namespace "urn:native-deviation"; prefix nd;
  import native { prefix n; }
  deviation "/n:root/n:holder/n:item/n:id" {
    deviate replace {
      type uint32 { range "1..64"; }
    }
  }
}
"#,
        );

        let dev_index =
            DeviationIndex::build(&[(ModuleKey::latest("native-deviation"), dev.clone())]);
        let mut reg = ModuleRegistry::new();
        for (name, stmt) in
            [("common", common), ("native", native), ("native-deviation", dev)]
        {
            reg.insert(Arc::new(compile_module(
                &ModuleKey::latest(name),
                stmt,
                &reg,
                &dev_index,
                &AnnotationIndex::default(),
                &AstAnnotationIndex::default(),
            )));
        }

        let module = reg.resolve_import("native", None).unwrap();
        let ctx = ExpansionCtx::all_features(&reg);

        let path = |s: &str| -> Vec<PathStep> {
            s.split('/')
                .filter(|p| !p.is_empty())
                .map(|name| PathStep { prefix: None, name: name.to_string() })
                .collect()
        };
        let range_of = |n: &SchemaNode| match &n.kind {
            SchemaNodeKind::Leaf { type_stmt, .. } => type_stmt
                .get_substmt(BuiltInKeyword::Range)
                .and_then(|s| s.arg.clone())
                .expect("range present"),
            other => panic!("expected leaf, got {other:?}"),
        };

        // The walker is self-sufficient: it threads the rooted (consuming-module)
        // overlay as the secondary, so it finds the deferred deviation even though
        // the descent crosses into `common`'s grouping body (whose own overlay is
        // empty). No emitter context needed.
        let (terminal, _) = walk_path_collecting_intermediates(
            &path("root/holder/item/id"),
            &module.children,
            &ctx,
        )
        .expect("path resolves");
        assert_eq!(
            range_of(&terminal),
            "1..64",
            "the walker must see the deviated range across a cross-module uses, not 1..512"
        );

        // Parity with the cs/main path under the emitter contract: with
        // `file_module_overlay` set to the consuming module's overlay (as the
        // emitter does), the level-by-level `children()` descent also deviates, and
        // the walker agrees.
        let native_mod = reg.resolve_import("native", None).unwrap();
        let emit_ctx =
            ExpansionCtx::all_features(&reg).with_file_module_overlay(&native_mod.overlay);
        let root = module.children.iter().find(|n| n.name == "root").unwrap();
        let holder = root.children(&emit_ctx).into_iter().find(|n| n.name == "holder").unwrap();
        let item = holder.children(&emit_ctx).into_iter().find(|n| n.name == "item").unwrap();
        let id = item.children(&emit_ctx).into_iter().find(|n| n.name == "id").unwrap();
        assert_eq!(range_of(&id), "1..64", "main path deviated under emitter contract");
        let (terminal2, _) = walk_path_collecting_intermediates(
            &path("root/holder/item/id"),
            &module.children,
            &emit_ctx,
        )
        .expect("path resolves");
        assert_eq!(range_of(&terminal2), "1..64", "walker matches the cs/main path");
    }

    #[test]
    fn scope_grouping_annotations_to_target_drops_cross_uses_must() {
        use crate::plugin::{AstOverlayDescriptor, ExtensionId};

        let parse = |name: &str, src: &str| {
            let (stmts, errs) = parse_yang(src, Arc::from(format!("{name}.yang").as_str()));
            assert!(errs.is_empty(), "parse {name}: {errs:?}");
            (ModuleKey::latest(name), stmts.into_iter().next().unwrap())
        };

        let base = parse(
            "base",
            r#"
module base {
  namespace "urn:base"; prefix b;
  grouping threshold-pair {
    leaf rising-threshold { type uint32; }
    leaf falling-threshold { type uint32; }
  }
  container native { uses threshold-pair; }
}
"#,
        );
        let ann = parse(
            "base-ann",
            r#"
module base-ann {
  yang-version 1.1;
  namespace "urn:base-ann"; prefix bann;
  import acme-ext { prefix acme; }
  import base { prefix b; }
  acme:annotate-module "base" {
    acme:annotate-statement "grouping[name='threshold-pair']" {
      acme:annotate-statement "leaf[name='falling-threshold']" {
        must "../rising-threshold" { error-message "needs rising"; }
        acme:cli-hidden "deprecated";
      }
    }
  }
}
"#,
        );
        let overlay = parse(
            "overlay",
            r#"
module overlay {
  namespace "urn:overlay"; prefix o;
  import base { prefix b; }
  augment "/b:native" {
    container also-here { uses b:threshold-pair; }
  }
}
"#,
        );

        let descs = vec![AstOverlayDescriptor {
            module_selector: ExtensionId { module: "acme-ext", name: "annotate-module" },
            stmt_selector: ExtensionId { module: "acme-ext", name: "annotate-statement" },
        }];
        let ast_ann = AstAnnotationIndex::build(&[ann.clone()], &descs);
        // Provenance must have resolved to the annotation module.
        assert!(ast_ann.is_annotation_module("base-ann"));

        // Count musts on the falling-threshold leaf.
        let musts_on = |node: &SchemaNode| match &node.kind {
            SchemaNodeKind::Leaf { musts, .. } => musts.clone(),
            other => panic!("expected leaf, got {other:?}"),
        };
        // Count the annotation-injected `cli-hidden` extension on a node.
        let hidden_exts = |node: &SchemaNode| -> usize {
            node.extensions.iter().filter(|e| e.name == "cli-hidden").count()
        };

        // Build the registry + check the leaf in both `native` (base) and the
        // overlay augment body, for a given value of the opt-in flag.
        // Returns (base_musts, overlay_musts, base_hidden, overlay_hidden, must_src).
        let run = |flag: bool| -> (usize, usize, usize, usize, String) {
            let mut reg = ModuleRegistry::new();
            reg.flags.scope_grouping_annotations_to_target = flag;
            for (key, stmt) in [base.clone(), overlay.clone()] {
                let patched = ast_ann.apply(stmt, &key.name);
                let compiled = compile_module(
                    &key,
                    patched,
                    &reg,
                    &DeviationIndex::default(),
                    &AnnotationIndex::default(),
                    &ast_ann,
                );
                reg.insert(Arc::new(compiled));
            }
            let ctx = ExpansionCtx::all_features(&reg).with_annotation_index(&ast_ann);

            // base: /native/falling-threshold
            let base_m = reg.resolve_import("base", None).unwrap();
            let native = base_m.children.iter().find(|n| n.name == "native").unwrap();
            let bf = native
                .children(&ctx)
                .into_iter()
                .find(|n| n.name == "falling-threshold")
                .unwrap();
            let base_musts = musts_on(&bf);

            // overlay: augment body /b:native/also-here/falling-threshold
            let overlay_m = reg.resolve_import("overlay", None).unwrap();
            let body = ctx.expand_augment_body(&overlay_m, &overlay_m.augments[0]);
            let also = body.iter().find(|n| n.name == "also-here").unwrap();
            let of = also
                .children(&ctx)
                .into_iter()
                .find(|n| n.name == "falling-threshold")
                .unwrap();
            let overlay_musts = musts_on(&of);

            let src = base_musts.first().map(|m| m.source_module.clone()).unwrap_or_default();
            (base_musts.len(), overlay_musts.len(), hidden_exts(&bf), hidden_exts(&of), src)
        };

        // Flag ON: the annotation targets `base`, so base keeps both the must and
        // the injected extension, but `overlay` — which only reuses the grouping —
        // must inherit neither.
        let (base_must, overlay_must, base_hidden, overlay_hidden, src) = run(true);
        assert_eq!(base_must, 1, "base keeps its annotation-injected must");
        assert_eq!(src, "base-ann", "must is attributed to the annotation module");
        assert_eq!(base_hidden, 1, "base keeps the annotation-injected extension");
        assert_eq!(overlay_must, 0, "overlay must NOT inherit the annotation must");
        assert_eq!(overlay_hidden, 0, "overlay must NOT inherit the annotation extension");

        // Flag OFF (default): standard YANG copies the grouping's body to every
        // consumer — unchanged behaviour.
        let (base_must, overlay_must, _, overlay_hidden, _) = run(false);
        assert_eq!(base_must, 1);
        assert_eq!(overlay_must, 1, "default: the must propagates through uses");
        assert_eq!(overlay_hidden, 1, "default: the extension propagates through uses");
    }

    #[test]
    fn scope_grouping_annotations_recognises_submodule_target() {
        // The annotation targets a *submodule* (`base-iface`, belongs-to `base`),
        // whose grouping is merged into the parent `base` at compile. Under the
        // flag, `base`'s own use of the grouping must KEEP the injected
        // must/extension (the annotation effectively targets base), while a foreign
        // module (`overlay`) reusing the grouping must still drop them.
        use crate::plugin::{AstOverlayDescriptor, ExtensionId};

        let parse = |name: &str, src: &str| {
            let (stmts, errs) = parse_yang(src, Arc::from(format!("{name}.yang").as_str()));
            assert!(errs.is_empty(), "parse {name}: {errs:?}");
            (ModuleKey::latest(name), stmts.into_iter().next().unwrap())
        };

        let iface = parse(
            "base-iface",
            r#"
submodule base-iface {
  belongs-to base { prefix b; }
  grouping threshold-pair {
    leaf rising-threshold { type uint32; }
    leaf falling-threshold { type uint32; }
  }
}
"#,
        );
        let base = parse(
            "base",
            r#"
module base {
  namespace "urn:base"; prefix b;
  include base-iface;
  container native { uses threshold-pair; }
}
"#,
        );
        let ann = parse(
            "base-ann",
            r#"
module base-ann {
  yang-version 1.1;
  namespace "urn:base-ann"; prefix bann;
  import acme-ext { prefix acme; }
  import base-iface { prefix bi; }
  acme:annotate-module "base-iface" {
    acme:annotate-statement "grouping[name='threshold-pair']" {
      acme:annotate-statement "leaf[name='falling-threshold']" {
        must "../rising-threshold" { error-message "needs rising"; }
        acme:cli-hidden "deprecated";
      }
    }
  }
}
"#,
        );
        let overlay = parse(
            "overlay",
            r#"
module overlay {
  namespace "urn:overlay"; prefix o;
  import base { prefix b; }
  augment "/b:native" {
    container also-here { uses b:threshold-pair; }
  }
}
"#,
        );

        let descs = vec![AstOverlayDescriptor {
            module_selector: ExtensionId { module: "acme-ext", name: "annotate-module" },
            stmt_selector: ExtensionId { module: "acme-ext", name: "annotate-statement" },
        }];
        // Pool includes the submodule (for the submodule→parent map) and the
        // annotation module (which declares the annotate-module).
        let ast_ann = AstAnnotationIndex::build(
            &[iface.clone(), base.clone(), ann.clone(), overlay.clone()],
            &descs,
        );
        // The fix: the submodule-targeting annotation also targets the parent.
        assert!(ast_ann.annotation_targets("base-ann", "base"));
        assert!(ast_ann.annotation_targets("base-ann", "base-iface"));
        assert!(!ast_ann.annotation_targets("base-ann", "overlay"));

        let leaf_musts = |node: &SchemaNode| match &node.kind {
            SchemaNodeKind::Leaf { musts, .. } => musts.len(),
            other => panic!("expected leaf, got {other:?}"),
        };
        let hidden = |node: &SchemaNode| node.extensions.iter().filter(|e| e.name == "cli-hidden").count();

        let mut reg = ModuleRegistry::new();
        reg.flags.scope_grouping_annotations_to_target = true;
        // Submodule before parent (parent merges its groupings via the registry).
        for (key, stmt) in [iface, base, overlay] {
            let patched = ast_ann.apply(stmt, &key.name);
            let compiled = compile_module(
                &key,
                patched,
                &reg,
                &DeviationIndex::default(),
                &AnnotationIndex::default(),
                &ast_ann,
            );
            reg.insert(Arc::new(compiled));
        }
        let ctx = ExpansionCtx::all_features(&reg).with_annotation_index(&ast_ann);

        // base/native/falling-threshold — must be KEPT (was over-dropped before).
        let base_m = reg.resolve_import("base", None).unwrap();
        let native = base_m.children.iter().find(|n| n.name == "native").unwrap();
        let bf = native
            .children(&ctx)
            .into_iter()
            .find(|n| n.name == "falling-threshold")
            .unwrap();
        assert_eq!(leaf_musts(&bf), 1, "parent keeps the submodule-targeted must");
        assert_eq!(hidden(&bf), 1, "parent keeps the submodule-targeted extension");

        // overlay/also-here/falling-threshold — still DROPPED (foreign module).
        let overlay_m = reg.resolve_import("overlay", None).unwrap();
        let body = ctx.expand_augment_body(&overlay_m, &overlay_m.augments[0]);
        let also = body.iter().find(|n| n.name == "also-here").unwrap();
        let of = also
            .children(&ctx)
            .into_iter()
            .find(|n| n.name == "falling-threshold")
            .unwrap();
        assert_eq!(leaf_musts(&of), 0, "foreign reuse still drops the must");
        assert_eq!(hidden(&of), 0, "foreign reuse still drops the extension");
    }

    #[test]
    fn annotation_targeting_parent_reaches_grouping_in_submodule() {
        // A `annotate-module "<parent>"` whose selectors descend into a grouping
        // that is actually defined in a *submodule* of that parent. The parent's
        // direct AST has no such grouping (it lives in the included submodule), so
        // the selector used to miss entirely and the injected must/extension were
        // lost on *every* expanded copy. The fix: a submodule also applies its
        // parent's selectors, since the named definition lives wherever it lives.
        use crate::plugin::{AstOverlayDescriptor, ExtensionId};

        let parse = |name: &str, src: &str| {
            let (stmts, errs) = parse_yang(src, Arc::from(format!("{name}.yang").as_str()));
            assert!(errs.is_empty(), "parse {name}: {errs:?}");
            (ModuleKey::latest(name), stmts.into_iter().next().unwrap())
        };

        let iface = parse(
            "base-iface",
            r#"
submodule base-iface {
  belongs-to base { prefix b; }
  grouping threshold-pair {
    leaf rising-threshold { type uint32; }
    leaf falling-threshold { type uint32; }
  }
}
"#,
        );
        let base = parse(
            "base",
            r#"
module base {
  namespace "urn:base"; prefix b;
  include base-iface;
  container native { uses threshold-pair; }
}
"#,
        );
        // Annotation targets the PARENT module name, not the submodule.
        let ann = parse(
            "base-ann",
            r#"
module base-ann {
  yang-version 1.1;
  namespace "urn:base-ann"; prefix bann;
  import acme-ext { prefix acme; }
  import base { prefix b; }
  acme:annotate-module "base" {
    acme:annotate-statement "grouping[name='threshold-pair']" {
      acme:annotate-statement "leaf[name='falling-threshold']" {
        must "../rising-threshold" { error-message "needs rising"; }
        acme:cli-hidden "deprecated";
      }
    }
  }
}
"#,
        );
        let consumer = parse(
            "consumer",
            r#"
module consumer {
  namespace "urn:consumer"; prefix c;
  import base { prefix b; }
  augment "/b:native" {
    container also-here { uses b:threshold-pair; }
  }
}
"#,
        );

        let descs = vec![AstOverlayDescriptor {
            module_selector: ExtensionId { module: "acme-ext", name: "annotate-module" },
            stmt_selector: ExtensionId { module: "acme-ext", name: "annotate-statement" },
        }];
        let ast_ann = AstAnnotationIndex::build(
            &[iface.clone(), base.clone(), ann.clone(), consumer.clone()],
            &descs,
        );

        let leaf_musts = |node: &SchemaNode| match &node.kind {
            SchemaNodeKind::Leaf { musts, .. } => musts.len(),
            other => panic!("expected leaf, got {other:?}"),
        };
        let hidden = |node: &SchemaNode| {
            node.extensions.iter().filter(|e| e.name == "cli-hidden").count()
        };

        let mut reg = ModuleRegistry::new();
        // Submodule before parent (parent merges its groupings via the registry).
        for (key, stmt) in [iface, base, consumer] {
            let patched = ast_ann.apply(stmt, &key.name);
            let compiled = compile_module(
                &key,
                patched,
                &reg,
                &DeviationIndex::default(),
                &AnnotationIndex::default(),
                &ast_ann,
            );
            reg.insert(Arc::new(compiled));
        }
        let ctx = ExpansionCtx::all_features(&reg).with_annotation_index(&ast_ann);

        // base/native/falling-threshold — the parent's own use of the submodule
        // grouping carries the injected must + extension.
        let base_m = reg.resolve_import("base", None).unwrap();
        let native = base_m.children.iter().find(|n| n.name == "native").unwrap();
        let bf = native
            .children(&ctx)
            .into_iter()
            .find(|n| n.name == "falling-threshold")
            .unwrap();
        assert_eq!(leaf_musts(&bf), 1, "parent-targeted must rides into base's use");
        assert_eq!(hidden(&bf), 1, "parent-targeted extension rides into base's use");

        // consumer augment reusing the grouping via `uses` — the expanded copy in
        // the consuming module also carries the injected must + extension.
        let consumer_m = reg.resolve_import("consumer", None).unwrap();
        let body = ctx.expand_augment_body(&consumer_m, &consumer_m.augments[0]);
        let also = body.iter().find(|n| n.name == "also-here").unwrap();
        let cf = also
            .children(&ctx)
            .into_iter()
            .find(|n| n.name == "falling-threshold")
            .unwrap();
        assert_eq!(leaf_musts(&cf), 1, "cross-module uses carries the must");
        assert_eq!(hidden(&cf), 1, "cross-module uses carries the extension");
    }

    #[test]
    fn submodule_must_be_in_ann_index_slice_for_parent_attribution() {
        // Regression guard mirroring the bundle-CLI pipeline. An
        // `annotate-module "<submodule>"` overlay is recognised as ALSO targeting the
        // submodule's *parent* (which `include`s and merges its groupings) only if the
        // submodule is present in `AstAnnotationIndex::build`'s slice — that's where its
        // `belongs-to` parent is read. In the bundle CLI, submodules arrive as
        // search-path deps and are NOT in `input_modules`, so `bin/src/main.rs` feeds
        // submodules into the index explicitly. Without them, `submodule_parents` is
        // empty, the parent is never added to `ann_effective_targets`, and the parent's
        // `uses` expansion wrongly filters the injected extension as foreign (the
        // `is_foreign_annotation_ext` path under `scope_grouping_annotations_to_target`).
        //
        // This pins the exact hinge the caller fix relies on; the end-to-end "extension
        // survives" half is covered by `scope_grouping_annotations_recognises_submodule_target`.
        use crate::plugin::{AstOverlayDescriptor, ExtensionId};

        let parse = |name: &str, src: &str| {
            let (stmts, errs) = parse_yang(src, Arc::from(format!("{name}.yang").as_str()));
            assert!(errs.is_empty(), "parse {name}: {errs:?}");
            (ModuleKey::latest(name), stmts.into_iter().next().unwrap())
        };

        let iface = parse(
            "base-iface",
            r#"
submodule base-iface {
  belongs-to base { prefix b; }
  grouping threshold-pair {
    leaf falling-threshold { type uint32; }
  }
}
"#,
        );
        let base = parse(
            "base",
            r#"
module base {
  namespace "urn:base"; prefix b;
  include base-iface;
  container native { uses threshold-pair; }
}
"#,
        );
        let ann = parse(
            "base-ann",
            r#"
module base-ann {
  yang-version 1.1;
  namespace "urn:base-ann"; prefix bann;
  import acme-ext { prefix acme; }
  import base-iface { prefix bi; }
  acme:annotate-module "base-iface" {
    acme:annotate-statement "grouping[name='threshold-pair']" {
      acme:annotate-statement "leaf[name='falling-threshold']" {
        acme:cli-hidden "deprecated";
      }
    }
  }
}
"#,
        );
        let descs = vec![AstOverlayDescriptor {
            module_selector: ExtensionId { module: "acme-ext", name: "annotate-module" },
            stmt_selector: ExtensionId { module: "acme-ext", name: "annotate-statement" },
        }];

        // Pre-fix bundle pipeline: submodule absent from the slice. The literal target
        // (the submodule) is still recognised, but the parent is NOT.
        let without = AstAnnotationIndex::build(&[base.clone(), ann.clone()], &descs);
        assert!(without.annotation_targets("base-ann", "base-iface"));
        assert!(
            !without.annotation_targets("base-ann", "base"),
            "without the submodule in the slice, the parent is unknown — the bug the \
             foreign-filter then acts on"
        );

        // What `bin/src/main.rs` now feeds: submodule included → parent recognised.
        let with = AstAnnotationIndex::build(&[iface, base, ann], &descs);
        assert!(
            with.annotation_targets("base-ann", "base"),
            "with the submodule in the slice, the parent is an effective target"
        );
    }

    #[test]
    fn when_must_in_submodule_grouping_attributed_to_submodule() {
        // A `when`/`must` written in a grouping inside a *submodule*, reached from
        // another module via the parent's prefix, must be attributed to the
        // submodule that authored it — not the parent it was reached through. The
        // tricky case is a nested/inline grouping (forces the fallback recompile
        // path), where the attribution used to collapse to the parent.
        let parse = |name: &str, src: &str| {
            let (stmts, errs) = parse_yang(src, Arc::from(format!("{name}.yang").as_str()));
            assert!(errs.is_empty(), "parse {name}: {errs:?}");
            (ModuleKey::latest(name), stmts.into_iter().next().unwrap())
        };

        let sub = parse(
            "par-sub",
            r#"
submodule par-sub {
  belongs-to par { prefix p; }
  grouping h { leaf z { type string; } }
  grouping g {
    grouping inner {
      container cc {
        when "true()";
        leaf x { type string; must "../y"; }
        leaf y { type string; }
      }
    }
    uses inner;
    uses h { when "1 = 1"; }   // when on the `uses` itself (RFC 7950 §7.13.2)
  }
}
"#,
        );
        let par = parse(
            "par",
            r#"module par { namespace "urn:par"; prefix p; include par-sub; }"#,
        );
        let cons = parse(
            "cons",
            r#"
module cons {
  namespace "urn:cons"; prefix c;
  import par { prefix p; }
  container use-it { uses p:g; }
}
"#,
        );

        let mut reg = ModuleRegistry::new();
        for (k, st) in [sub, par, cons] {
            let comp = compile_module(
                &k,
                st,
                &reg,
                &DeviationIndex::default(),
                &AnnotationIndex::default(),
                &AstAnnotationIndex::default(),
            );
            reg.insert(Arc::new(comp));
        }
        let ctx = ExpansionCtx::all_features(&reg);
        let cmod = reg.resolve_import("cons", None).unwrap();
        let use_it = cmod.children.iter().find(|n| n.name == "use-it").unwrap();
        let cc = use_it
            .children(&ctx)
            .into_iter()
            .find(|n| n.name == "cc")
            .expect("cc reached via nested submodule grouping");

        // `when` on the container is attributed to the submodule.
        assert_eq!(cc.when.len(), 1);
        assert_eq!(
            cc.when[0].source_module, "par-sub",
            "submodule `when` must not collapse to the parent"
        );
        // `must` on the leaf likewise.
        let x = cc.children(&ctx).into_iter().find(|n| n.name == "x").unwrap();
        let musts = match &x.kind {
            SchemaNodeKind::Leaf { musts, .. } => musts.clone(),
            other => panic!("expected leaf, got {other:?}"),
        };
        assert_eq!(musts.len(), 1);
        assert_eq!(musts[0].source_module, "par-sub", "submodule `must` likewise");

        // `when` written on the `uses h` statement itself (not inside h's body) is
        // also attributed to the authoring submodule — it propagates onto h's
        // top-level nodes during expansion.
        let z = use_it
            .children(&ctx)
            .into_iter()
            .find(|n| n.name == "z")
            .expect("z reached via `uses h` with a when");
        assert_eq!(z.when.len(), 1);
        assert_eq!(
            z.when[0].source_module, "par-sub",
            "`when` on a `uses` in a submodule grouping must attribute to the submodule"
        );
    }

    #[test]
    fn refine_must_appends_to_grouping_musts() {
        // RFC 7950 §7.13.2: a refine's `must` is ADDED to the target's existing
        // `must` list, not replaced. A grouping leaf with two musts, used with a
        // refine adding a third, must expand to a leaf carrying all three.
        let stmt = parse_module(
            r#"
module m {
  namespace "urn:m";
  prefix m;
  grouping g {
    leaf l {
      type string;
      must "../a";
      must "../b";
    }
  }
  container c {
    uses g {
      refine "l" {
        must "../d";
      }
    }
  }
}
"#,
        );
        let compiled = compile_module(
            &ModuleKey::latest("m"),
            stmt,
            &ModuleRegistry::default(),
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
            &AstAnnotationIndex::default(),
        );
        assert!(compiled.errors.is_empty(), "compile errors: {:?}", compiled.errors);

        let mut reg = ModuleRegistry::new();
        reg.insert(Arc::new(compiled));
        let module = reg.resolve_import("m", None).unwrap();
        let ctx = ExpansionCtx::all_features(&reg);
        // Expand `c`'s children (drives the `uses g { refine }` expansion).
        let c = module
            .children
            .iter()
            .find(|n| n.name == "c")
            .expect("container c");
        let kids = c.children(&ctx);
        let l = kids.iter().find(|n| n.name == "l").expect("leaf l from grouping");
        let musts = match &l.kind {
            SchemaNodeKind::Leaf { musts, .. } => musts,
            other => panic!("expected leaf, got {other:?}"),
        };
        let xpaths: Vec<&str> = musts.iter().map(|m| m.xpath.as_str()).collect();
        assert_eq!(
            xpaths,
            vec!["../a", "../b", "../d"],
            "refine must must append after the grouping's existing musts, not replace them"
        );
    }

    #[test]
    fn refine_if_feature_appends_to_grouping_if_features() {
        // RFC 7950 §7.13.2: a refine's `if-feature` is ADDED to the target's
        // existing `if-feature` list, not replaced (and not dropped). A grouping
        // leaf guarded by `f1`, used with a refine adding `if-feature f2`, must
        // expand to a leaf carrying both.
        let stmt = parse_module(
            r#"
module m {
  namespace "urn:m";
  prefix m;
  feature f1;
  feature f2;
  grouping g {
    leaf l {
      type string;
      if-feature f1;
    }
  }
  container c {
    uses g {
      refine "l" {
        if-feature f2;
      }
    }
  }
}
"#,
        );
        let compiled = compile_module(
            &ModuleKey::latest("m"),
            stmt,
            &ModuleRegistry::default(),
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
            &AstAnnotationIndex::default(),
        );
        assert!(compiled.errors.is_empty(), "compile errors: {:?}", compiled.errors);

        let mut reg = ModuleRegistry::new();
        reg.insert(Arc::new(compiled));
        let module = reg.resolve_import("m", None).unwrap();
        let ctx = ExpansionCtx::all_features(&reg);
        let c = module
            .children
            .iter()
            .find(|n| n.name == "c")
            .expect("container c");
        let kids = c.children(&ctx);
        let l = kids.iter().find(|n| n.name == "l").expect("leaf l from grouping");
        let names: Vec<&str> = l
            .if_features
            .iter()
            .map(|f| match f {
                IfFeatureExpr::Name(n, _) => n.as_str(),
                _ => panic!("expected a bare feature name, got {f:?}"),
            })
            .collect();
        assert_eq!(
            names,
            vec!["f1", "f2"],
            "refine if-feature must append after the grouping's existing if-features, \
             not drop or replace them"
        );
    }

    #[test]
    fn extracts_opaque_type_and_info_when_flags_configured() {
        // A backend plugin declares (via Plugin::configure_compilation) that the
        // compiler should extract two typedef extensions: an opaque-type marker
        // and an info string. With the flags set, compilation populates
        // Typedef::{has_opaque_type, opaque_type_name, ext_info}. Without them
        // (the default), those stay empty even though the extensions are present.
        let stmt = parse_module(
            r#"
module m {
  namespace "urn:m";
  prefix m;
  import acme-ext { prefix acme; }
  typedef opaque-prefix {
    acme:info "human readable";
    acme:typepoint "opaque_name";
    type string { pattern 'x.*'; }
  }
}
"#,
        );

        // Default flags: extensions present but not extracted.
        let plain = compile_module(
            &ModuleKey::latest("m"),
            stmt.clone(),
            &ModuleRegistry::default(),
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
            &AstAnnotationIndex::default(),
        );
        let td = plain.typedefs.get("opaque-prefix").expect("typedef present");
        assert!(!td.has_opaque_type, "no extraction without flags");
        assert_eq!(td.opaque_type_name, None);
        assert_eq!(td.ext_info, None);

        // Flags configured exactly as Plugin::configure_compilation would set them.
        let mut reg = ModuleRegistry::default();
        reg.flags.opaque_type_extension = Some(("acme-ext".into(), "typepoint".into()));
        reg.flags.typedef_info_extension = Some(("acme-ext".into(), "info".into()));
        let compiled = compile_module(
            &ModuleKey::latest("m"),
            stmt,
            &reg,
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
            &AstAnnotationIndex::default(),
        );
        let td = compiled.typedefs.get("opaque-prefix").expect("typedef present");
        assert!(td.has_opaque_type, "opaque-type extension extracted");
        assert_eq!(td.opaque_type_name.as_deref(), Some("opaque_name"));
        assert_eq!(td.ext_info.as_deref(), Some("human readable"));
    }

    #[test]
    fn configure_compilation_hook_sets_flags() {
        // The Plugin trait hook is the seam the binary uses to apply each
        // plugin's required flags before compilation.
        use crate::plugin::Plugin;

        struct OpaqueBackend;
        impl Plugin for OpaqueBackend {
            fn name(&self) -> &'static str {
                "opaque-backend"
            }
            fn configure_compilation(&self, flags: &mut CompilationFlags) {
                flags.opaque_type_extension = Some(("acme-ext".into(), "typepoint".into()));
                flags.typedef_info_extension = Some(("acme-ext".into(), "info".into()));
            }
        }

        let mut flags = CompilationFlags::default();
        assert!(flags.opaque_type_extension.is_none());
        OpaqueBackend.configure_compilation(&mut flags);
        assert_eq!(
            flags.opaque_type_extension,
            Some(("acme-ext".into(), "typepoint".into()))
        );
        assert_eq!(
            flags.typedef_info_extension,
            Some(("acme-ext".into(), "info".into()))
        );
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
            &AstAnnotationIndex::default(),
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
            &AstAnnotationIndex::default(),
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
            &AstAnnotationIndex::default(),
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
            &AstAnnotationIndex::default(),
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
            &AstAnnotationIndex::default(),
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
            &AstAnnotationIndex::default(),
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
            &AstAnnotationIndex::default(),
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
            &AstAnnotationIndex::default(),
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
            &AstAnnotationIndex::default(),
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
            &AstAnnotationIndex::default(),
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
            &AstAnnotationIndex::default(),
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
            &AstAnnotationIndex::default(),
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

    // ── #5 source-position augment ordering ──────────────────────────────────

    fn aug_entry(file: &str, line: u32) -> AugmentEntry {
        AugmentEntry {
            target_path: vec![],
            nodes: vec![],
            when: vec![],
            if_features: vec![],
            status: Status::Current,
            pos: Pos::FilePos { file: Arc::from(file), line },
        }
    }

    #[test]
    fn sort_augments_orders_by_source_position_stably() {
        // Out-of-order by line, plus two sharing a position to check stability.
        let mut augments = vec![
            aug_entry("b.yang", 5), // 0
            aug_entry("a.yang", 30), // 1
            aug_entry("a.yang", 10), // 2
            aug_entry("a.yang", 10), // 3 (same pos as 2 — must stay after it)
        ];
        // Tag stable identity via nodes length is awkward; use `when` len as a marker.
        augments[3].when.push(WhenExpr {
            xpath: "marker".into(),
            description: None,
            reference: None,
            source_module: String::new(),
            source_revision: None,
            non_local: false,
            explicit_deps: vec![],
            override_auto_deps: false,
        });

        sort_augments_by_source(&mut augments);

        let order: Vec<(&str, u32)> = augments
            .iter()
            .map(|a| (a.pos.orig_file().as_ref(), a.pos.orig_line()))
            .collect();
        assert_eq!(
            order,
            vec![("a.yang", 10), ("a.yang", 10), ("a.yang", 30), ("b.yang", 5)]
        );
        // Stability: the marked entry (originally index 3) stays after the
        // unmarked same-position entry.
        assert!(augments[0].when.is_empty());
        assert_eq!(augments[1].when.len(), 1);
    }

    #[test]
    fn overlay_key_disambiguates_by_leaf_module_with_fallback() {
        fn marker(tag: &str) -> Stmt {
            Stmt::new(
                Keyword::BuiltIn(BuiltInKeyword::Leaf),
                Some(tag.to_string()),
                Pos::FilePos { file: Arc::from("t"), line: 1 },
                vec![],
            )
        }
        let mut map = NodeOverlayMap::new();
        let path = vec!["top".to_string(), "iface".to_string()];
        // Two same-named-sibling targets from different modules (e.g. two augments).
        overlay_entry_mut(&mut map, path.clone(), Some("modA".into()), |e| {
            e.deviate_stmts.push(marker("A"))
        });
        overlay_entry_mut(&mut map, path.clone(), Some("modB".into()), |e| {
            e.deviate_stmts.push(marker("B"))
        });

        // A node in modA gets only modA's entry (disambiguated — no collision).
        let a = lookup_overlay(&map, &path, "modA").unwrap();
        assert_eq!(a.deviate_stmts.len(), 1);
        assert_eq!(a.deviate_stmts[0].arg.as_deref(), Some("A"));
        // A node in modB gets only modB's entry.
        let b = lookup_overlay(&map, &path, "modB").unwrap();
        assert_eq!(b.deviate_stmts.len(), 1);
        assert_eq!(b.deviate_stmts[0].arg.as_deref(), Some("B"));
        // A node whose module matches neither falls back to the name-only (`None`)
        // bucket — preserving the legacy grouping-expansion behaviour.
        let c = lookup_overlay(&map, &path, "modC").unwrap();
        assert_eq!(c.deviate_stmts.len(), 2, "fallback bucket aggregates both");
    }

    #[test]
    fn collected_augments_carry_source_position_in_order() {
        // Two external augments: each `pos` reflects its source line and the
        // stored list is in source order.
        let stmt = parse_module(
            r#"
module m {
  namespace "urn:m";
  prefix m;
  import dep { prefix d; }
  augment "/d:first" {
    leaf x { type string; }
  }
  augment "/d:second" {
    leaf y { type string; }
  }
}
"#,
        );
        let compiled = compile_module(
            &ModuleKey::latest("m"),
            stmt,
            &ModuleRegistry::default(),
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
            &AstAnnotationIndex::default(),
        );
        assert_eq!(compiled.augments.len(), 2, "both augments are external");
        let lines: Vec<u32> = compiled.augments.iter().map(|a| a.pos.orig_line()).collect();
        assert!(lines[0] < lines[1], "augments ordered by source line: {lines:?}");
        // Targets preserved in source order.
        assert_eq!(compiled.augments[0].target_path.last().unwrap().name, "first");
        assert_eq!(compiled.augments[1].target_path.last().unwrap().name, "second");
    }
}
