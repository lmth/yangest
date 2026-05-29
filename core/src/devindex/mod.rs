// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Deviation index construction (DEVINDEX phase).
//!
//! Scans raw `Stmt` trees for `deviation` statements and groups pending
//! deviations by the name of the target module.  At this stage we only have
//! the raw argument string (an absolute schema node id whose first path
//! component carries a prefix).  The target module name is resolved at
//! devindex build time from the deviating module's raw import statements, so
//! no registry lookup is needed in the compile phase.

use std::collections::HashMap;

use crate::ast::{BuiltInKeyword, ModuleKey, Pos, Stmt};

// ── Public types ──────────────────────────────────────────────────────────────

/// A deviation statement waiting to be applied to its target module.
#[derive(Debug, Clone)]
pub struct PendingDeviation {
    /// The module that declared this deviation (i.e., the deviating module).
    pub from_module: ModuleKey,
    /// The raw `deviation` argument string, e.g. `"/ex:interfaces/ex:interface"`.
    pub target_path: String,
    /// The prefix used on the first step of `target_path`.
    /// `None` if the first step has no prefix (targets same module).
    pub target_prefix: Option<String>,
    /// Resolved name of the target module, pre-computed from the deviating
    /// module's import stmts.  `None` when there is no prefix (same module).
    pub target_module_name: Option<String>,
    /// The prefix used on the LAST step of `target_path`.
    /// For cross-module augment deviations (e.g. `/A:foo/B:bar`), this is B's prefix.
    /// `None` if the last step has no prefix.
    pub target_leaf_prefix: Option<String>,
    /// Resolved name of the module owning the leaf node (last step's module).
    /// For cross-module augment deviations this differs from `target_module_name`.
    /// This is the module where the deviation is applied in `apply_deviations`,
    /// matching how `expand_children` routes overlay entries by the last prefix.
    pub target_leaf_module_name: Option<String>,
    /// Full prefix→module_name map from the deviating module's imports.
    /// Used to resolve intermediate path step prefixes without a registry lookup.
    pub prefix_map: HashMap<String, String>,
    /// Source position of the `deviation` statement.
    pub pos: Pos,
    /// The raw `deviate` sub-statements (add/replace/delete/not-supported).
    pub deviate_stmts: Vec<Stmt>,
}

/// Maps a **deviating module** → all pending deviations it declares.
#[derive(Debug, Default)]
pub struct DeviationIndex {
    pub by_deviating_module: HashMap<ModuleKey, Vec<PendingDeviation>>,
}

impl DeviationIndex {
    /// Build from a list of (key, stmt_tree) pairs (the same list used for
    /// `DepGraph::build`).
    pub fn build(modules: &[(ModuleKey, Stmt)]) -> Self {
        Self::build_from_refs(&modules.iter().collect::<Vec<_>>())
    }

    /// Build from a filtered list of references.  Used by the binary to
    /// restrict deviation application to explicitly-listed input files only.
    pub fn build_from_refs(modules: &[&(ModuleKey, Stmt)]) -> Self {
        let mut index = DeviationIndex::default();

        for (key, stmt) in modules {
            let deviations = extract_deviations(key, stmt);
            if !deviations.is_empty() {
                index.by_deviating_module.insert(key.clone(), deviations);
            }
        }

        index
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Build a raw prefix→module_name map from a module/submodule's import stmts.
fn raw_prefix_map(stmt: &Stmt) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for import_stmt in stmt.get_substmts(BuiltInKeyword::Import) {
        if let Some(module_name) = import_stmt.arg.clone() {
            if let Some(prefix_stmt) = import_stmt.get_substmt(BuiltInKeyword::Prefix) {
                if let Some(prefix) = prefix_stmt.arg.clone() {
                    map.insert(prefix, module_name);
                }
            }
        }
    }
    map
}

fn extract_deviations(key: &ModuleKey, stmt: &Stmt) -> Vec<PendingDeviation> {
    let prefix_map = raw_prefix_map(stmt);
    let mut result = Vec::new();
    collect_deviations(key, stmt, &prefix_map, &mut result);
    result
}

fn collect_deviations(
    key: &ModuleKey,
    stmt: &Stmt,
    prefix_map: &HashMap<String, String>,
    out: &mut Vec<PendingDeviation>,
) {
    for sub in &stmt.substmts {
        if sub.keyword.is_builtin(BuiltInKeyword::Deviation) {
            let target_path = sub.arg_str().to_string();
            let target_prefix = extract_first_prefix(&target_path);
            let target_module_name = target_prefix
                .as_deref()
                .and_then(|pfx| prefix_map.get(pfx))
                .cloned();
            let target_leaf_prefix = extract_last_prefix(&target_path);
            let target_leaf_module_name = target_leaf_prefix
                .as_deref()
                .and_then(|pfx| prefix_map.get(pfx))
                .cloned();
            let deviate_stmts: Vec<Stmt> = sub
                .get_substmts(BuiltInKeyword::Deviate)
                .cloned()
                .collect();

            out.push(PendingDeviation {
                from_module: key.clone(),
                target_path,
                target_prefix,
                target_module_name,
                target_leaf_prefix,
                target_leaf_module_name,
                prefix_map: prefix_map.clone(),
                pos: sub.pos.clone(),
                deviate_stmts,
            });
        }
    }
}

/// Extract the prefix from the first path step of an absolute schema node id.
///
/// `/ex:interfaces/ex:interface` → `Some("ex")`  
/// `/interfaces`                 → `None`  
fn extract_first_prefix(path: &str) -> Option<String> {
    let path = path.trim_start_matches('/');
    let first_step = path.split('/').next()?;
    if let Some(colon_pos) = first_step.find(':') {
        Some(first_step[..colon_pos].to_string())
    } else {
        None
    }
}

/// Extract the prefix from the last path step of an absolute schema node id.
///
/// `/A:foo/B:bar/B:baz` → `Some("B")`  
/// `/A:foo/A:bar`       → `Some("A")`  
/// `/A:foo/bar`         → `None` (last step has no prefix)
fn extract_last_prefix(path: &str) -> Option<String> {
    let path = path.trim_start_matches('/');
    let last_step = path.split('/').next_back()?;
    if let Some(colon_pos) = last_step.find(':') {
        Some(last_step[..colon_pos].to_string())
    } else {
        None
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Keyword, Pos};
    use std::sync::Arc;

    fn file() -> Arc<str> {
        Arc::from("test.yang")
    }

    fn make_deviation_module(
        deviator_name: &str,
        target_path: &str,
    ) -> (ModuleKey, Stmt) {
        let pos = Pos::new(file(), 1);
        let deviate = Stmt::new(
            Keyword::BuiltIn(BuiltInKeyword::Deviate),
            Some("not-supported".to_string()),
            pos.clone(),
            vec![],
        );
        let deviation = Stmt::new(
            Keyword::BuiltIn(BuiltInKeyword::Deviation),
            Some(target_path.to_string()),
            pos.clone(),
            vec![deviate],
        );
        let module = Stmt::new(
            Keyword::BuiltIn(BuiltInKeyword::Module),
            Some(deviator_name.to_string()),
            pos,
            vec![deviation],
        );
        (ModuleKey::latest(deviator_name), module)
    }

    #[test]
    fn extract_prefix_from_path() {
        assert_eq!(extract_first_prefix("/ex:interfaces"), Some("ex".to_string()));
        assert_eq!(extract_first_prefix("/ex:a/ex:b"), Some("ex".to_string()));
        assert_eq!(extract_first_prefix("/interfaces"), None);
    }

    #[test]
    fn build_index_finds_deviations() {
        let modules = vec![make_deviation_module(
            "my-deviations",
            "/ex:interfaces/ex:interface",
        )];
        let idx = DeviationIndex::build(&modules);
        let key = ModuleKey::latest("my-deviations");
        let devs = idx.by_deviating_module.get(&key).unwrap();
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].target_prefix, Some("ex".to_string()));
        assert_eq!(devs[0].target_module_name, None); // no import for "ex" in the test module
        assert_eq!(devs[0].deviate_stmts.len(), 1);
    }
}

