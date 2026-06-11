// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! AST-level annotation index.
//!
//! Scans annotation modules for plugin-declared *module-selector* extensions
//! (e.g. `acme:annotate-module`) and their nested *statement-selector*
//! extensions (e.g. `acme:annotate-statement`), then builds a per-module
//! patch set.  Each patch is applied to the target module's raw [`Stmt`] tree
//! **before** [`compile_module`] is called, making injected statements visible
//! to type resolution, grouping instantiation, and constraint checking.
//!
//! Unlike [`crate::annindex`] — which targets expanded data-node paths (schema
//! nodeids) and is applied at `uses` expansion time — the `AstAnnotationIndex`
//! targets *source* statements: groupings, typedefs, and the module statement
//! itself.  These have no schema-nodeid, so they cannot be reached via the
//! existing path-based overlay mechanism.

use std::collections::{HashMap, HashSet};

use crate::ast::{BuiltInKeyword, Keyword, ModuleKey, Stmt};
use crate::plugin::AstOverlayDescriptor;

// ── Public types ──────────────────────────────────────────────────────────────

/// A parsed `annotate-statement`-style selector: identifies a child statement
/// by keyword (and optional name predicate) and carries the statements to
/// inject into every matching node, plus further nested selectors.
#[derive(Debug, Clone)]
pub struct StmtSelector {
    /// Built-in keyword of the target statement (e.g. `Grouping`, `List`).
    pub keyword: BuiltInKeyword,
    /// If present, only match statements whose argument equals this value.
    pub name_predicate: Option<String>,
    /// Statements to append to each matching statement's sub-statement list.
    pub direct_injections: Vec<Stmt>,
    /// Nested selectors applied within each matching statement.
    pub nested: Vec<StmtSelector>,
}

/// All AST-level patches that target a single module.
#[derive(Debug, Default, Clone)]
pub struct ModuleAnnotation {
    /// Statements to append directly to the module (or submodule) statement body.
    pub direct_injections: Vec<Stmt>,
    /// Statement selectors to apply within the module.
    pub selectors: Vec<StmtSelector>,
}

/// Maps module names to their accumulated AST-level patches.
///
/// Built once after all overlay modules are parsed; passed as a shared
/// read-only reference into the per-module compile workers, where
/// [`AstAnnotationIndex::apply`] is called before `compile_module`.
#[derive(Debug, Default)]
pub struct AstAnnotationIndex {
    by_module: HashMap<String, ModuleAnnotation>,
    /// Maps target module name → list of `(ann_module_key, prefix_map)` for
    /// every annotation module that targets it via `acme:annotate-module`.
    /// Used by plugins to look up which annotation modules contributed
    /// extension-arg imports to a given target module.
    ann_sources: HashMap<String, Vec<(ModuleKey, HashMap<String, String>)>>,
    /// Maps annotation module source file path → `ModuleKey` for that annotation module.
    /// Used during compilation to identify which annotation module injected a particular
    /// statement (must/when) by checking `stmt.pos.file()` against this map.
    file_to_key: HashMap<String, ModuleKey>,
    /// Maps annotation-module name → the set of modules it effectively targets.
    /// This is the *literal* `annotate-module` argument plus, when that argument
    /// names a **submodule**, the submodule's parent module — because the parent
    /// merges the submodule's definitions, so an annotation targeting the
    /// submodule applies to the parent's compilation. Consulted only by
    /// [`annotation_targets`](Self::annotation_targets) (the
    /// `scope_grouping_annotations_to_target` filter); the `ann_sources` /
    /// `sources_for` view keeps the literal targets unchanged.
    ann_effective_targets: HashMap<String, HashSet<String>>,
}

impl AstAnnotationIndex {
    /// Build an index from the given overlay-module `(key, stmt)` pairs and
    /// the set of registered [`AstOverlayDescriptor`]s.
    ///
    /// Returns an empty index immediately when `descriptors` is empty.
    pub fn build(modules: &[(ModuleKey, Stmt)], descriptors: &[AstOverlayDescriptor]) -> Self {
        if descriptors.is_empty() {
            return Self::default();
        }
        let mut index = Self::default();
        // submodule name → parent module name (from `belongs-to`). An annotation
        // targeting a submodule also targets its parent, since the parent merges
        // the submodule's definitions at compile time.
        let submodule_parents: HashMap<&str, String> = modules
            .iter()
            .filter(|(_, s)| s.keyword.is_builtin(BuiltInKeyword::Submodule))
            .filter_map(|(k, s)| {
                s.get_substmt(BuiltInKeyword::BelongsTo)
                    .and_then(|bt| bt.arg.clone())
                    .map(|parent| (k.name.as_str(), parent))
            })
            .collect();

        let mut seen: HashSet<&ModuleKey> = HashSet::new();
        for (key, stmt) in modules {
            if !seen.insert(key) {
                continue; // same module parsed twice (e.g. as annotation-module and search-path dep)
            }
            let prefix_map = raw_prefix_map(stmt);
            let own_pfx = own_prefix(stmt);
            for sub in &stmt.substmts {
                let Some(desc) =
                    find_module_selector(sub, &prefix_map, &own_pfx, &key.name, descriptors)
                else {
                    continue;
                };
                let target_module = match sub.arg.as_deref() {
                    Some(a) if !a.is_empty() => a.to_string(),
                    _ => continue,
                };
                let ann =
                    parse_module_annotation(&sub.substmts, &prefix_map, &own_pfx, &key.name, desc);
                let entry = index.by_module.entry(target_module.clone()).or_default();
                entry.direct_injections.extend(ann.direct_injections);
                entry.selectors.extend(ann.selectors);
                // Track the annotation module source for import resolution.
                index
                    .ann_sources
                    .entry(target_module.clone())
                    .or_default()
                    .push((key.clone(), prefix_map.clone()));
                // Track the annotation file path → module key, so that compilation
                // can detect which annotation module injected a given statement by
                // checking stmt.pos.orig_file() against this map.
                let file_path = stmt.pos.file().to_string();
                index.file_to_key.entry(file_path).or_insert_with(|| key.clone());
                // Effective targets: the literal target, plus its parent module if
                // the literal target is a submodule.
                let effective = index.ann_effective_targets.entry(key.name.clone()).or_default();
                if let Some(parent) = submodule_parents.get(target_module.as_str()) {
                    effective.insert(parent.clone());
                }
                effective.insert(target_module);
            }
        }
        index
    }

    /// Apply accumulated patches to `stmt` (a module or submodule statement),
    /// returning the (possibly modified) statement.
    ///
    /// If no patches target `module_name` this is a no-op and the original
    /// `stmt` is returned unchanged.
    pub fn apply(&self, mut stmt: Stmt, module_name: &str) -> Stmt {
        // A submodule's definitions belong to its parent's schema, so a parent
        // annotation's *selectors* (e.g. `grouping[name='g']/…`) may target a
        // grouping/typedef actually defined in this submodule. Resolve the parent
        // up-front (immutable borrow) before mutating `stmt.substmts` below.
        let parent = if stmt.keyword.is_builtin(BuiltInKeyword::Submodule) {
            stmt.get_substmt(BuiltInKeyword::BelongsTo)
                .and_then(|bt| bt.arg.clone())
        } else {
            None
        };

        // This module's own annotations: direct module-body injections + selectors.
        if let Some(ann) = self.by_module.get(module_name) {
            stmt.substmts.extend(ann.direct_injections.iter().cloned());
            for sel in &ann.selectors {
                apply_selector(&mut stmt.substmts, sel);
            }
        }

        // For a submodule, also apply the parent's *selectors* (its direct module
        // injections belong to the parent module statement, applied when the
        // parent itself is processed). A selector matches wherever the named
        // definition lives, so this is a no-op unless the parent annotation
        // targets something defined in this submodule.
        if let Some(ann) = parent.as_deref().and_then(|p| self.by_module.get(p)) {
            for sel in &ann.selectors {
                apply_selector(&mut stmt.substmts, sel);
            }
        }
        stmt
    }

    /// Returns the list of `(ann_module_key, prefix_map)` for every annotation
    /// module that targets `module_name` via `acme:annotate-module`.
    pub fn sources_for(&self, module_name: &str) -> &[(ModuleKey, HashMap<String, String>)] {
        self.ann_sources.get(module_name).map_or(&[], Vec::as_slice)
    }

    /// Returns the `ModuleKey` of the annotation module whose source file matches `file`,
    /// or `None` if the file is not from any annotation module.
    ///
    /// Use `stmt.pos.orig_file()` (not `stmt.pos.file()`) as the key to handle statements
    /// that were injected into a grouping and then expanded via `uses` — in that case
    /// `pos.file()` returns the uses-call site, but `pos.orig_file()` returns the
    /// original definition (annotation module) file.
    pub fn module_key_for_file(&self, file: &str) -> Option<&ModuleKey> {
        self.file_to_key.get(file)
    }

    /// True if `module_name` is an annotation module (declares `annotate-module`).
    /// Used to tell an annotation-injected `must`/`when` (whose `source_module` is
    /// the annotation module) from a native one when scoping constraints to their
    /// target module.
    pub fn is_annotation_module(&self, module_name: &str) -> bool {
        self.file_to_key.values().any(|k| k.name == module_name)
    }

    /// True if the annotation module `ann_module` targets `target` via
    /// `annotate-module` — directly, or because `target`'s submodule was the
    /// literal `annotate-module` argument (the parent merges the submodule's
    /// definitions, so the annotation applies to the parent's compilation).
    pub fn annotation_targets(&self, ann_module: &str, target: &str) -> bool {
        self.ann_effective_targets
            .get(ann_module)
            .is_some_and(|targets| targets.contains(target))
    }

    pub fn is_empty(&self) -> bool {
        self.by_module.is_empty()
    }
}

// ── Index building helpers ────────────────────────────────────────────────────

fn raw_prefix_map(stmt: &Stmt) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for import_stmt in stmt.get_substmts(BuiltInKeyword::Import) {
        if let Some(module_name) = &import_stmt.arg {
            if let Some(prefix_stmt) = import_stmt.get_substmt(BuiltInKeyword::Prefix) {
                if let Some(prefix) = &prefix_stmt.arg {
                    map.insert(prefix.clone(), module_name.clone());
                }
            }
        }
    }
    map
}

fn own_prefix(stmt: &Stmt) -> String {
    stmt.get_substmt(BuiltInKeyword::Prefix)
        .and_then(|p| p.arg.as_deref())
        .unwrap_or("")
        .to_string()
}

/// Resolve an extension keyword in the annotation module's prefix context.
fn resolve_ext_kw(
    kw: &Keyword,
    prefix_map: &HashMap<String, String>,
    own_pfx: &str,
    own_module: &str,
) -> Option<(String, String)> {
    match kw {
        Keyword::ExtensionPrefixed { prefix, name } => {
            let module = if prefix == own_pfx {
                own_module.to_string()
            } else {
                prefix_map.get(prefix.as_str())?.clone()
            };
            Some((module, name.clone()))
        }
        Keyword::Extension { module, name } => Some((module.clone(), name.clone())),
        _ => None,
    }
}

fn find_module_selector<'d>(
    sub: &Stmt,
    prefix_map: &HashMap<String, String>,
    own_pfx: &str,
    own_module: &str,
    descriptors: &'d [AstOverlayDescriptor],
) -> Option<&'d AstOverlayDescriptor> {
    let (m, n) = resolve_ext_kw(&sub.keyword, prefix_map, own_pfx, own_module)?;
    descriptors
        .iter()
        .find(|d| d.module_selector.module == m && d.module_selector.name == n)
}

fn is_stmt_selector(
    sub: &Stmt,
    prefix_map: &HashMap<String, String>,
    own_pfx: &str,
    own_module: &str,
    desc: &AstOverlayDescriptor,
) -> bool {
    match resolve_ext_kw(&sub.keyword, prefix_map, own_pfx, own_module) {
        Some((m, n)) => desc.stmt_selector.module == m && desc.stmt_selector.name == n,
        None => false,
    }
}

fn parse_module_annotation(
    body: &[Stmt],
    prefix_map: &HashMap<String, String>,
    own_pfx: &str,
    own_module: &str,
    desc: &AstOverlayDescriptor,
) -> ModuleAnnotation {
    let mut ann = ModuleAnnotation::default();
    for sub in body {
        if is_stmt_selector(sub, prefix_map, own_pfx, own_module, desc) {
            if let Some(sel) = parse_stmt_selector(sub, prefix_map, own_pfx, own_module, desc) {
                ann.selectors.push(sel);
            }
        } else {
            ann.direct_injections.push(sub.clone());
        }
    }
    ann
}

fn parse_stmt_selector(
    sub: &Stmt,
    prefix_map: &HashMap<String, String>,
    own_pfx: &str,
    own_module: &str,
    desc: &AstOverlayDescriptor,
) -> Option<StmtSelector> {
    let selector_str = sub.arg.as_deref()?.trim();
    let (kw_str, name_pred) = parse_selector_string(selector_str)?;
    // Only built-in keywords are supported as selector targets; extension
    // keywords in selectors would require the target module's prefix map,
    // which is unavailable at index-build time.
    let keyword = BuiltInKeyword::from_str(&kw_str)?;

    let mut direct = Vec::new();
    let mut nested = Vec::new();
    for body_sub in &sub.substmts {
        if is_stmt_selector(body_sub, prefix_map, own_pfx, own_module, desc) {
            if let Some(ns) = parse_stmt_selector(body_sub, prefix_map, own_pfx, own_module, desc)
            {
                nested.push(ns);
            }
        } else {
            direct.push(body_sub.clone());
        }
    }
    Some(StmtSelector { keyword, name_predicate: name_pred, direct_injections: direct, nested })
}

/// Parse `"keyword[name='value']"` or `"keyword"` into `(kw_str, predicate)`.
fn parse_selector_string(s: &str) -> Option<(String, Option<String>)> {
    if let Some(bracket_pos) = s.find('[') {
        let kw = s[..bracket_pos].trim().to_string();
        if kw.is_empty() {
            return None;
        }
        let inside = s[bracket_pos + 1..].trim();
        let inside = inside.strip_suffix(']').unwrap_or(inside).trim();
        let name_val = inside
            .strip_prefix("name=")
            .map(|v| v.trim().trim_matches('\'').to_string());
        Some((kw, name_val))
    } else {
        let kw = s.trim().to_string();
        if kw.is_empty() { None } else { Some((kw, None)) }
    }
}

// ── Application helpers ───────────────────────────────────────────────────────

fn apply_selector(stmts: &mut Vec<Stmt>, sel: &StmtSelector) {
    for stmt in stmts.iter_mut() {
        if !stmt.keyword.is_builtin(sel.keyword) {
            continue;
        }
        if let Some(pred) = &sel.name_predicate {
            if stmt.arg.as_deref() != Some(pred.as_str()) {
                continue;
            }
        }
        stmt.substmts.extend(sel.direct_injections.iter().cloned());
        for nested in &sel.nested {
            apply_selector(&mut stmt.substmts, nested);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Keyword, Pos};

    fn pos() -> Pos {
        Pos::FilePos { file: std::sync::Arc::from("test"), line: 1 }
    }

    fn leaf(name: &str) -> Stmt {
        Stmt::new(
            Keyword::BuiltIn(BuiltInKeyword::Leaf),
            Some(name.to_string()),
            pos(),
            vec![],
        )
    }

    fn grouping(name: &str, children: Vec<Stmt>) -> Stmt {
        Stmt::new(
            Keyword::BuiltIn(BuiltInKeyword::Grouping),
            Some(name.to_string()),
            pos(),
            children,
        )
    }

    fn module_stmt(children: Vec<Stmt>) -> Stmt {
        Stmt::new(
            Keyword::BuiltIn(BuiltInKeyword::Module),
            Some("target-mod".to_string()),
            pos(),
            children,
        )
    }

    /// Build a minimal index directly, bypassing YANG parsing.
    fn make_index(
        module_name: &str,
        direct: Vec<Stmt>,
        selectors: Vec<StmtSelector>,
    ) -> AstAnnotationIndex {
        let mut idx = AstAnnotationIndex::default();
        idx.by_module
            .insert(module_name.to_string(), ModuleAnnotation { direct_injections: direct, selectors });
        idx
    }

    #[test]
    fn direct_injection_into_module() {
        let extra = leaf("injected");
        let idx = make_index("target-mod", vec![extra.clone()], vec![]);
        let m = module_stmt(vec![leaf("original")]);
        let result = idx.apply(m, "target-mod");
        assert_eq!(result.substmts.len(), 2);
        assert_eq!(result.substmts[1].arg.as_deref(), Some("injected"));
    }

    #[test]
    fn selector_matches_by_keyword_and_name() {
        let extra = leaf("injected");
        let sel = StmtSelector {
            keyword: BuiltInKeyword::Grouping,
            name_predicate: Some("my-group".to_string()),
            direct_injections: vec![extra],
            nested: vec![],
        };
        let idx = make_index("target-mod", vec![], vec![sel]);
        let m = module_stmt(vec![
            grouping("my-group", vec![leaf("original")]),
            grouping("other-group", vec![]),
        ]);
        let result = idx.apply(m, "target-mod");
        // my-group gets injection; other-group does not
        let my = &result.substmts[0];
        assert_eq!(my.substmts.len(), 2);
        assert_eq!(my.substmts[1].arg.as_deref(), Some("injected"));
        let other = &result.substmts[1];
        assert_eq!(other.substmts.len(), 0);
    }

    #[test]
    fn selector_no_predicate_matches_all() {
        let extra = leaf("injected");
        let sel = StmtSelector {
            keyword: BuiltInKeyword::Grouping,
            name_predicate: None,
            direct_injections: vec![extra],
            nested: vec![],
        };
        let idx = make_index("target-mod", vec![], vec![sel]);
        let m = module_stmt(vec![grouping("g1", vec![]), grouping("g2", vec![])]);
        let result = idx.apply(m, "target-mod");
        assert!(result.substmts.iter().all(|s| s.substmts.len() == 1));
    }

    #[test]
    fn parse_selector_string_with_predicate() {
        let (kw, pred) = parse_selector_string("grouping[name='my-group']").unwrap();
        assert_eq!(kw, "grouping");
        assert_eq!(pred.as_deref(), Some("my-group"));
    }

    #[test]
    fn parse_selector_string_without_predicate() {
        let (kw, pred) = parse_selector_string("list").unwrap();
        assert_eq!(kw, "list");
        assert!(pred.is_none());
    }

    #[test]
    fn no_match_for_unknown_module() {
        let idx = make_index("other-mod", vec![leaf("injected")], vec![]);
        let m = module_stmt(vec![]);
        let result = idx.apply(m, "target-mod");
        assert_eq!(result.substmts.len(), 0);
    }
}
