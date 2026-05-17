//! FXS and YANG header term construction.

use std::collections::BTreeMap;

use yangest_core::ast::{BuiltInKeyword, Keyword};
use yangest_core::compiler::{AppliedAnnotations, AppliedDeviations, CompiledModule, ExpansionCtx, ModuleRegistry};

use crate::hash::phash2_atom;
use crate::terms::{
    atom, binary_str, bigint, charlist, list, make_fxs_header, make_yang_header, nil,
    tuple, undefined,
};

// fxs_header.flags constants (from cs.hrl)
const F_FXS_ALLOW_ENUM_CONFLICTS: u32 = 1 << 1;
const F_FXS_IGNORE_DEPS: u32 = 1 << 2;
const F_FXS_IS_NCS_DEVICE_MODEL: u32 = 1 << 3;
const F_FXS_NEW_FORMAT: u32 = 1 << 4;
const F_FXS_HAS_CDB: u32 = 1 << 5;
const F_FXS_HAS_CDB_OPER: u32 = 1 << 6;
const F_FXS_FEATURE_WHEN_DEPENDENT: u32 = 1 << 8;

// yang_header.flags constants
const F_YANG_IS_IMPLEMENTED: u32 = 1 << 0;

/// Build the `{fxs_header, ...}` placeholder term (with zero checksums).
///
/// The checksum, cdb_checksum, and sections fields will be patched after the
/// data section is written.  `has_cdb` is true when any cs node in the module
/// is CDB-backed.  `dummy_sections` should use zero byte-offsets for any
/// section positions — they will be patched to real values in
/// `build_fxs_header_final`.
pub fn build_fxs_header_placeholder(
    module: &CompiledModule,
    registry: &ModuleRegistry,
    yang_header: eetf::Term,
    model_sizes: eetf::Term,
    augments: eetf::Term,
    has_cdb: bool,
    has_cdb_oper: bool,
    dummy_sections: eetf::Term,
) -> eetf::Term {
    let ns = &module.namespace;
    let ns_atom = atom(ns);

    let id_hash = phash2_atom(ns);

    let mut flags: u32 =
        F_FXS_ALLOW_ENUM_CONFLICTS | F_FXS_IGNORE_DEPS | F_FXS_NEW_FORMAT | F_FXS_FEATURE_WHEN_DEPENDENT;
    let is_ncs_device_model = module.stmt.substmts.iter().any(|s| {
        matches!(&s.keyword, Keyword::Extension { module: m, name: n }
            if m == "tailf-common" && n == "ncs-device-type")
    });
    if is_ncs_device_model {
        flags |= F_FXS_IS_NCS_DEVICE_MODEL;
    }
    if has_cdb {
        flags |= F_FXS_HAS_CDB;
    }
    if has_cdb_oper {
        flags |= F_FXS_HAS_CDB_OPER;
    }

    // ns_dependencies: namespaces of all used imported modules (excluding cisco-semver etc.)
    let ns_deps = build_ns_dependencies(module, registry);

    // ns_to_prefix_maps: built from module.prefix_map
    let ns_to_prefix_maps = build_ns_to_prefix_maps(module, registry);

    let checksum_zero = eetf::Term::from(eetf::Binary { bytes: vec![0u8; 16] });
    let mount_id = nil();

    make_fxs_header(
        ns_atom.clone(),          // id
        ns_atom.clone(),          // uri
        ns_atom.clone(),          // xmlns
        atom("cs"),               // type
        build_exported_agents(module), // exported_agents
        charlist(&module.prefix),    // prefix (charlist, not atom)
        bigint(id_hash as u128),  // id_hash_value
        ns_deps,
        checksum_zero.clone(),    // checksum placeholder
        bigint(flags as u128),    // flags
        yang_header,
        model_sizes,
        checksum_zero,            // cdb_checksum placeholder
        augments,
        dummy_sections,
        mount_id,
        ns_to_prefix_maps,
    )
}

/// Build the final `{fxs_header, ...}` term with real checksums and section positions.
pub fn build_fxs_header_final(
    placeholder: &eetf::Term,
    cdb_checksum: [u8; 16],
    full_checksum: [u8; 16],
    sections: eetf::Term,
) -> eetf::Term {
    // Clone placeholder and patch the checksum and sections fields.
    // fxs_header tuple positions (0-indexed): record=0 key=1 id=2 uri=3 type=4
    // exported_agents=5 prefix=6 id_hash_value=7 ns_dependencies=8 checksum=9
    // flags=10 mountpoint=11 tid=12 snmp_info=13 yang_header=14
    // merged_revisions=15 model_sizes=16 cdb_checksum=17 augments=18
    // sections=19 mount_id=20 xmlns=21 ns_to_prefix_maps=22
    if let eetf::Term::Tuple(t) = placeholder {
        let mut elements = t.elements.clone();
        elements[9] = eetf::Term::from(eetf::Binary { bytes: full_checksum.to_vec() });
        elements[17] = eetf::Term::from(eetf::Binary { bytes: cdb_checksum.to_vec() });
        elements[19] = sections;
        eetf::Term::from(eetf::Tuple { elements })
    } else {
        placeholder.clone()
    }
}

/// Build the `exported_agents` term for the FXS header.
///
/// In yanger_fxs, `tailf:export <agent>` at module level restricts which agents
/// can access the module's schema. Multiple `tailf:export` stmts → list of atoms.
/// No `tailf:export` → `all` (unrestricted).
fn build_exported_agents(module: &CompiledModule) -> eetf::Term {
    // Find the prefix(es) used for tailf-common in this module (via prefix_map).
    // The raw AST uses ExtensionPrefixed { prefix, name }, NOT Extension { module, name }.
    let tailf_prefixes: Vec<&str> = module
        .prefix_map
        .iter()
        .filter_map(|(pfx, mod_name)| {
            if mod_name == "tailf-common" {
                Some(pfx.as_str())
            } else {
                None
            }
        })
        .collect();

    let agents: Vec<eetf::Term> = module
        .stmt
        .substmts
        .iter()
        .filter_map(|s| match &s.keyword {
            Keyword::ExtensionPrefixed { prefix: p, name: n }
                if tailf_prefixes.contains(&p.as_str()) && n == "export" =>
            {
                s.arg.as_deref().map(atom)
            }
            Keyword::Extension { module: m, name: n }
                if m == "tailf-common" && n == "export" =>
            {
                s.arg.as_deref().map(atom)
            }
            _ => None,
        })
        .collect();

    if agents.is_empty() {
        atom("all")
    } else {
        list(agents)
    }
}

/// Build the `ns_to_prefix_maps` term from a module's prefix map.
///
/// Structure: `[{{NsAtom, NameAtom, Revision}, [{NsAtom, PrefixCharlist}...]}]`
///
/// In yanger, ns_to_prefix_map entries are accumulated dynamically during schema
/// node processing (for leafref targets and XPath/when/must expressions).  Here we
/// emit only the main module entry, which is always present.  Additional entries
/// (submodules, imported modules referenced in leafrefs, annotation modules with
/// XPath arguments) require full schema-node processing to determine correctly and
/// are left for a future improvement.
pub fn build_ns_to_prefix_maps(module: &CompiledModule, registry: &ModuleRegistry) -> eetf::Term {
    let own_ns = &module.namespace;

    // Main module entry: {own_ns, module_name, revision} → [{ns, prefix}...]
    let mut ns_to_prefix: BTreeMap<String, String> = BTreeMap::new();
    ns_to_prefix.insert(own_ns.clone(), module.prefix.clone());
    for (prefix, mod_name) in &module.prefix_map {
        if let Some(imported) = registry.resolve_import(mod_name, None) {
            if !imported.namespace.is_empty() {
                ns_to_prefix.insert(imported.namespace.clone(), prefix.clone());
            }
        }
    }
    let map_list = list(
        ns_to_prefix.iter()
            .map(|(ns, prefix)| tuple(vec![atom(ns), charlist(prefix)]))
            .collect(),
    );
    let rev_term = match &module.key.revision {
        Some(rev) => binary_str(rev),
        None => undefined(),
    };
    let id = tuple(vec![atom(own_ns), atom(&module.key.name), rev_term]);
    let main_entry = tuple(vec![id, map_list]);

    list(vec![main_entry])
}

/// Build the `{yang_header, ...}` term from a `CompiledModule`.
pub fn build_yang_header(module: &CompiledModule, ctx: &ExpansionCtx<'_>, registry: &ModuleRegistry) -> eetf::Term {
    let yang_version = match module.yang_version {
        yangest_core::compiler::YangVersion::V1 => atom("1"),
        yangest_core::compiler::YangVersion::V11 => atom("1.1"),
    };

    // Revision from the module statement's revision sub-statement
    let revision = get_module_revision(module);

    // Features: only enabled features, in reverse-alphabetical order.
    // yanger_fxs accumulates via prepend ([Name | Acc]) while folding over
    // features in Erlang term (alphabetical) order, producing a reversed list.
    let mut enabled_feature_names: Vec<&str> = module
        .features
        .keys()
        .filter(|name| is_feature_enabled(ctx, &module.key.name, name))
        .map(String::as_str)
        .collect();
    enabled_feature_names.sort_unstable_by(|a, b| b.cmp(a)); // reverse alphabetical
    let features = list(enabled_feature_names.iter().map(|n| binary_str(n)).collect());

    // Deviations: modules that deviate this module (applied_deviations).
    let deviations = list(
        module.pdata::<AppliedDeviations>()
            .map(|d| d.0.as_slice())
            .unwrap_or(&[])
            .iter().map(|(name, rev)| {
            let rev_term = match rev {
                Some(r) => binary_str(r),
                None => binary_str(""),
            };
            tuple(vec![binary_str(name), rev_term])
        }).collect()
    );

    let module_name = binary_str(&module.key.name);

    // F_YANG_IS_IMPLEMENTED: set when the module implements data nodes, features,
    // augments, or deviations — mirroring yanger_fxs mk_yang_header.
    let has_deviations = module.stmt.substmts.iter()
        .any(|s| matches!(&s.keyword, yangest_core::ast::Keyword::BuiltIn(BuiltInKeyword::Deviation)));
    let has_augments = !module.augments.is_empty();
    let is_implemented = !module.children.is_empty()
        || !enabled_feature_names.is_empty()
        || has_augments
        || has_deviations;
    let flags: u32 = if is_implemented { F_YANG_IS_IMPLEMENTED } else { 0 };

    // Submodules: list of included submodules with their revisions.
    // Matches yanger_fxs's mk_yang_header: [{<<SubName>>, SubRevision}...]
    let submodules = {
        let sub_terms: Vec<eetf::Term> = module.includes.iter().filter_map(|sub_name| {
            let sub_mod = registry.resolve_import(sub_name, None)?;
            let rev_term = match &sub_mod.key.revision {
                Some(r) => binary_str(r),
                None => binary_str(""),
            };
            Some(tuple(vec![binary_str(sub_name), rev_term]))
        }).collect();
        list(sub_terms)
    };

    // Imports: include all used imports except annotation-only modules (cisco-semver)
    // and deviation target modules (for deviation modules).
    let imports = build_yang_header_imports(module, registry);

    make_yang_header(
        yang_version,
        revision,
        features,
        deviations,
        module_name,
        bigint(flags as u128),
        submodules,
        imports,
    )
}

/// Build the yang_header imports term.
///
/// Mirrors yanger_fxs's logic: include all used imports except:
///  - annotation-only modules (cisco-semver)
///  - deviation target modules (for modules containing `deviation` statements)
///
/// Format: `[{module_name_atom, actual_revision_binary, prefix_atom, required_rev_or_undefined}]`
/// A resolved import entry used for both yang_header.imports and fxs_header.ns_dependencies.
struct ResolvedImport {
    mod_name:     String,
    actual_rev:   Option<String>,
    prefix:       String,
    required_rev: Option<String>,
    namespace:    Option<String>,
}

/// Collect all `prefix:` references used anywhere in a statement's argument
/// or its sub-statements, recursively.
///
/// Importantly: extension keyword prefixes (e.g. `oc-ext` in `oc-ext:regexp-posix {}`)
/// are NOT collected.  Yanger only marks an import as "used" when `resolve_raw_idref`
/// is called, which happens for type/identity/leafref/XPath argument references — not
/// for extension keyword invocations.  Excluding keyword prefixes here mirrors that
/// behaviour: an import used only as an extension keyword is treated as "unused".
fn collect_prefixes_in_stmt(stmt: &yangest_core::ast::Stmt, prefixes: &mut std::collections::HashSet<String>) {
    if let Some(arg) = &stmt.arg {
        // Find all `word:word` patterns, but skip content inside single-quoted
        // XPath string literals — e.g. `when "if:type = 'ianaift:ethernetCsmacd'"` uses
        // the string literal `'ianaift:ethernetCsmacd'` for string comparison, not as
        // a namespace-qualified reference, so the prefix should not be counted as used.
        let stripped = strip_single_quoted_content(arg);
        for part in stripped.split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_' && c != ':') {
            if let Some(colon) = part.find(':') {
                let pfx = &part[..colon];
                if !pfx.is_empty() && pfx.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                    prefixes.insert(pfx.to_string());
                }
            }
        }
    }
    for sub in &stmt.substmts {
        collect_prefixes_in_stmt(sub, prefixes);
    }
}

/// Replace content between single quotes with spaces, to avoid counting
/// XPath string literals like `'prefix:value'` as namespace references.
fn strip_single_quoted_content(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_quotes = false;
    for ch in s.chars() {
        if ch == '\'' {
            in_quotes = !in_quotes;
            result.push(' ');
        } else if in_quotes {
            result.push(' ');
        } else {
            result.push(ch);
        }
    }
    result
}

/// Collect every prefix used in the statement's argument and sub-statements,
/// but skip `Deviation` statements entirely — all prefix references inside
/// deviation statements are `DeviationImports` in yanger's terminology and
/// should not be counted as "used" (they are excluded from the final import list).
fn collect_prefixes_in_stmt_non_devtarget(
    stmt: &yangest_core::ast::Stmt,
    prefixes: &mut std::collections::HashSet<String>,
) {
    use yangest_core::ast::BuiltInKeyword;
    // Skip entire `deviation` statements — both their target-path arg and all
    // substmts (deviate/must/type etc.) may reference foreign module prefixes
    // that should map to DeviationImports and be excluded.
    if matches!(&stmt.keyword, yangest_core::ast::Keyword::BuiltIn(BuiltInKeyword::Deviation)) {
        return;
    }
    collect_prefixes_in_stmt(stmt, prefixes);
}

/// Compute the set of "used" imports: all imports except annotation-only modules
/// (cisco-semver) and imports whose prefix is never referenced anywhere in the
/// module's AST.  Result is sorted alphabetically by module name (matching
/// yanger_fxs's `lists:usort(UsedImports)`).
fn collect_used_imports(module: &CompiledModule, registry: &ModuleRegistry) -> Vec<ResolvedImport> {
    use yangest_core::ast::BuiltInKeyword;

    /// Collect used prefixes from a single module's AST statements (main module or submodule).
    fn collect_used_prefixes_for(stmt: &yangest_core::ast::Stmt) -> std::collections::HashSet<String> {
        use yangest_core::ast::BuiltInKeyword;
        let mut prefixes = std::collections::HashSet::new();
        for s in &stmt.substmts {
            if matches!(&s.keyword,
                yangest_core::ast::Keyword::BuiltIn(BuiltInKeyword::Import) |
                yangest_core::ast::Keyword::BuiltIn(BuiltInKeyword::Include))
            {
                continue;
            }
            collect_prefixes_in_stmt_non_devtarget(s, &mut prefixes);
        }
        prefixes
    }

    // Collect used prefixes from the main module.
    let mut used_prefixes = collect_used_prefixes_for(&module.stmt);

    // Also collect used prefixes from included submodules, mirroring yanger_fxs's
    // `ModuleAndSubs = [M | submodules]` pattern.  Submodule type/leafref/XPath
    // references must count as "used" for the parent module's import list.
    for submod_name in &module.includes {
        if let Some(submod) = registry.resolve_import(submod_name, None) {
            let sub_prefixes = collect_used_prefixes_for(&submod.stmt);
            used_prefixes.extend(sub_prefixes);
        }
    }

    const ANNOTATION_ONLY: &[&str] = &["cisco-semver", "cisco-semver-internal"];
    // tailf-common is always kept when declared: yanger applies tailf annotation files
    // during compilation which makes tailf effectively "used" for all modules that import it.
    const ALWAYS_INCLUDE: &[&str] = &["tailf-common", "tailf-xsd-types"];

    let mut seen_mod_names: std::collections::HashSet<String> = Default::default();
    let mut result = Vec::new();

    /// Collect imports from one module's (or submodule's) import statements.
    fn collect_module_imports(
        stmt: &yangest_core::ast::Stmt,
        used_prefixes: &std::collections::HashSet<String>,
        seen_mod_names: &mut std::collections::HashSet<String>,
        result: &mut Vec<ResolvedImport>,
        registry: &ModuleRegistry,
        parent_includes: &[String],
        annotation_only: &[&str],
        always_include: &[&str],
    ) {
        use yangest_core::ast::BuiltInKeyword;
        for imp_stmt in stmt.substmts.iter()
            .filter(|s| matches!(&s.keyword, yangest_core::ast::Keyword::BuiltIn(BuiltInKeyword::Import)))
        {
            let mod_name = match &imp_stmt.arg {
                Some(n) => n.clone(),
                None => continue,
            };
            if annotation_only.contains(&mod_name.as_str()) {
                continue;
            }
            // Skip if already added (e.g. same module imported by both main module and submodule).
            if seen_mod_names.contains(&mod_name) {
                continue;
            }
            // Skip submodules of the parent module — they are not external dependencies.
            if parent_includes.contains(&mod_name) {
                continue;
            }
            let prefix = imp_stmt.get_substmt(BuiltInKeyword::Prefix)
                .and_then(|s| s.arg.clone())
                .unwrap_or_else(|| mod_name.clone());

            // Skip imports whose prefix is never referenced anywhere in the module or
            // its submodules, unless the module is always kept (tailf-common and friends).
            if !always_include.contains(&mod_name.as_str()) && !used_prefixes.contains(&prefix) {
                continue;
            }

            let required_rev: Option<String> = imp_stmt.get_substmt(BuiltInKeyword::RevisionDate)
                .and_then(|s| s.arg.clone());
            let imported_mod = registry.resolve_import(&mod_name, required_rev.as_deref());
            let actual_rev = imported_mod.as_ref().and_then(|m| m.key.revision.clone());
            let namespace = imported_mod.as_ref().map(|m| m.namespace.clone());
            seen_mod_names.insert(mod_name.clone());
            result.push(ResolvedImport { mod_name, actual_rev, prefix, required_rev, namespace });
        }
    }

    // Include imports from the main module's own import statements.
    collect_module_imports(
        &module.stmt, &used_prefixes, &mut seen_mod_names, &mut result,
        registry, &module.includes, ANNOTATION_ONLY, ALWAYS_INCLUDE,
    );

    // Include imports from each included submodule.  Mirrors yanger_fxs's
    // `ModuleAndSubs = [M | submodules]` iteration in `get_used_imports`.
    for submod_name in &module.includes {
        if let Some(submod) = registry.resolve_import(submod_name, None) {
            collect_module_imports(
                &submod.stmt, &used_prefixes, &mut seen_mod_names, &mut result,
                registry, &module.includes, ANNOTATION_ONLY, ALWAYS_INCLUDE,
            );
        }
    }

    // Also include imports from applied annotation modules.
    // Mirrors yanger_fxs's AnnotationImports mechanism: annotation modules' imports
    // become dependencies of the target module (e.g. the annotation imports a module
    // that its XPath path arguments reference).
    for (ann_name, _ann_rev, ann_prefix_map) in module
        .pdata::<AppliedAnnotations>()
        .map(|a| a.0.as_slice())
        .unwrap_or(&[])
    {
        for (ann_prefix, ann_mod_name) in ann_prefix_map {
            if ANNOTATION_ONLY.contains(&ann_mod_name.as_str()) {
                continue;
            }
            if seen_mod_names.contains(ann_mod_name) {
                continue;
            }
            if ann_mod_name == ann_name {
                // Don't add the annotation module itself as a dependency.
                continue;
            }
            if ann_mod_name == &module.key.name {
                // Don't add the target module (this module) as its own dependency.
                continue;
            }
            if module.includes.contains(ann_mod_name) {
                // Don't add a submodule of this module as an external dependency.
                continue;
            }
            let imported_mod = registry.resolve_import(ann_mod_name, None);
            let actual_rev = imported_mod.as_ref().and_then(|m| m.key.revision.clone());
            let namespace = imported_mod.as_ref().map(|m| m.namespace.clone());
            seen_mod_names.insert(ann_mod_name.clone());
            result.push(ResolvedImport {
                mod_name: ann_mod_name.clone(),
                actual_rev,
                prefix: ann_prefix.clone(),
                required_rev: None,
                namespace,
            });
        }
    }

    // Sort alphabetically by module name, matching yanger_fxs's `lists:usort(UsedImports)`.
    result.sort_by(|a, b| a.mod_name.cmp(&b.mod_name));
    result
}

fn build_yang_header_imports(module: &CompiledModule, registry: &ModuleRegistry) -> eetf::Term {
    let imports = collect_used_imports(module, registry);
    let terms: Vec<eetf::Term> = imports.iter().map(|i| {
        let actual_rev_term = match &i.actual_rev {
            Some(r) => binary_str(r),
            None => undefined(),
        };
        let required_rev_term = match &i.required_rev {
            Some(r) => binary_str(r),
            None => undefined(),
        };
        tuple(vec![atom(&i.mod_name), actual_rev_term, atom(&i.prefix), required_rev_term])
    }).collect();
    list(terms)
}

/// Build fxs_header.ns_dependencies: list of namespace atoms of used imported modules.
///
/// Mirrors yanger_fxs's `NsDependencies` computation: same as yang_header.imports
/// but additionally excludes `tailf-common` and `tailf-xsd-types` (the `?TAILF` and
/// `?XS` filters in yanger_fxs `get_imports`).
pub fn build_ns_dependencies(module: &CompiledModule, registry: &ModuleRegistry) -> eetf::Term {
    // These modules are excluded from ns_dependencies (but not from yang_header.imports).
    const NS_DEPS_EXCLUDE: &[&str] = &["tailf-common", "tailf-xsd-types"];

    let imports = collect_used_imports(module, registry);
    let terms: Vec<eetf::Term> = imports.iter()
        .filter(|i| !NS_DEPS_EXCLUDE.contains(&i.mod_name.as_str()))
        .filter_map(|i| i.namespace.as_deref().map(atom))
        .collect();
    list(terms)
}

/// Returns true if a feature is enabled under the current expansion context.
///
/// Delegates to `ctx.feature_enabled()` which handles per-module restrictions:
/// if the bundle [features] section does not mention a module at all, all its
/// features are enabled (only explicitly listed modules are restricted).
fn is_feature_enabled(ctx: &ExpansionCtx<'_>, module_name: &str, feature_name: &str) -> bool {
    ctx.feature_enabled(module_name, feature_name)
}

fn get_module_revision(module: &CompiledModule) -> eetf::Term {
    use yangest_core::ast::BuiltInKeyword;
    if let Some(rev_stmt) = module.stmt.get_substmt(BuiltInKeyword::Revision) {
        if let Some(date) = &rev_stmt.arg {
            return binary_str(date);
        }
    }
    undefined()
}
