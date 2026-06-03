// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Dependency graph construction (DEPGRAPH phase).
//!
//! Reads raw `Stmt` trees, extracts `import` and `include` edges, and builds a
//! topologically-sorted work queue.  Only `import`/`include` are edges — a
//! module that carries `deviation` statements is NOT a dependency of the
//! modules it deviates.

use std::collections::{HashMap, HashSet};

use crate::ast::{BuiltInKeyword, ModuleKey, Stmt, YError};

// ── Public types ──────────────────────────────────────────────────────────────

/// One node in the dependency graph, corresponding to one parsed module.
#[derive(Debug)]
pub struct DepNode {
    pub key: ModuleKey,
    /// Modules that `self` imports (direct import edges).
    pub imports: Vec<ModuleKey>,
    /// Submodules that `self` includes.
    pub includes: Vec<ModuleKey>,
    /// Modules that `self` deviates (not a dependency — stored for DEVINDEX).
    pub deviates: Vec<ModuleKey>,
    /// Modules that augment a node in `self` (not a dependency).
    pub augmented_by: Vec<ModuleKey>,
}

/// The complete dependency graph.
pub struct DepGraph {
    pub nodes: HashMap<ModuleKey, DepNode>,
    /// Input file order — the order modules were presented to `build`.
    /// `topo_sort` uses this as the DFS entry-point order (depth-first from the
    /// input files).
    pub input_order: Vec<ModuleKey>,
}

impl DepGraph {
    /// Build the dependency graph from a list of (key, stmt_tree) pairs.
    pub fn build(modules: &[(ModuleKey, Stmt)], errors: &mut Vec<YError>) -> Self {
        let mut nodes = HashMap::new();
        let mut input_order = Vec::with_capacity(modules.len());

        for (key, stmt) in modules {
            let node = extract_deps(key, stmt, errors);
            nodes.insert(key.clone(), node);
            input_order.push(key.clone());
        }

        DepGraph { nodes, input_order }
    }

    /// Compute a topologically sorted compilation order using DFS.
    ///
    /// Visits input files in the order they were provided (alphabetical path
    /// sort by the caller), recursively compiling each module's imports before
    /// the module itself. This produces a deterministic augment-application
    /// ordering.
    ///
    /// Returns `Ok(order)` where `order[0]` has no dependencies and can be
    /// compiled first.  Returns `Err(cycle)` if a cycle is detected.
    pub fn topo_sort(&self) -> Result<Vec<ModuleKey>, Vec<ModuleKey>> {
        let present: HashSet<&ModuleKey> = self.nodes.keys().collect();

        // Name-only index: module name → best (highest revision) key present.
        let mut name_to_key: HashMap<&str, &ModuleKey> = HashMap::new();
        for key in present.iter() {
            let entry = name_to_key.entry(key.name.as_str()).or_insert(*key);
            if entry.revision.is_none() && key.revision.is_some() {
                *entry = *key;
            }
        }

        let mut visited: HashSet<ModuleKey> = HashSet::new();
        let mut in_progress: HashSet<ModuleKey> = HashSet::new();
        let mut order: Vec<ModuleKey> = Vec::with_capacity(self.nodes.len());
        let mut cycle: Vec<ModuleKey> = Vec::new();

        for key in &self.input_order {
            if !visited.contains(key) {
                dfs_visit(
                    key,
                    &self.nodes,
                    &name_to_key,
                    &present,
                    &mut visited,
                    &mut in_progress,
                    &mut order,
                    &mut cycle,
                );
            }
        }

        if order.len() == self.nodes.len() {
            Ok(order)
        } else {
            Err(cycle)
        }
    }

    /// Like `topo_sort`, but groups modules into parallel compilation waves.
    /// All modules in wave N can be compiled concurrently because every one of
    /// their (transitive) dependencies is in wave 0..N-1.
    /// Returns `Err(cycle)` on cyclic dependency (same as `topo_sort`).
    pub fn topo_sort_levels(&self) -> Result<Vec<Vec<ModuleKey>>, Vec<ModuleKey>> {
        let flat = self.topo_sort()?;

        let present: HashSet<&ModuleKey> = self.nodes.keys().collect();
        let mut name_to_key: HashMap<&str, &ModuleKey> = HashMap::new();
        for key in present.iter() {
            let entry = name_to_key.entry(key.name.as_str()).or_insert(*key);
            if entry.revision.is_none() && key.revision.is_some() {
                *entry = *key;
            }
        }

        let mut level_map: HashMap<&ModuleKey, usize> = HashMap::with_capacity(flat.len());
        for key in &flat {
            let level = self
                .nodes
                .get(key)
                .map(|node| {
                    node.imports
                        .iter()
                        .chain(node.includes.iter())
                        .filter_map(|dep| {
                            let resolved = if present.contains(dep) {
                                dep
                            } else {
                                name_to_key.get(dep.name.as_str()).copied()?
                            };
                            level_map.get(resolved).map(|&lvl| lvl + 1)
                        })
                        .max()
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            level_map.insert(key, level);
        }

        let max_level = level_map.values().copied().max().unwrap_or(0);
        let mut waves = vec![Vec::new(); max_level + 1];
        for key in &flat {
            let level = *level_map.get(key).unwrap_or(&0);
            waves[level].push(key.clone());
        }
        Ok(waves)
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// DFS helper: visit `key` and all its transitive imports before appending
/// `key` to `order` (post-order DFS = dependency-first).
fn dfs_visit<'a>(
    key: &ModuleKey,
    nodes: &'a HashMap<ModuleKey, DepNode>,
    name_to_key: &HashMap<&str, &'a ModuleKey>,
    present: &HashSet<&'a ModuleKey>,
    visited: &mut HashSet<ModuleKey>,
    in_progress: &mut HashSet<ModuleKey>,
    order: &mut Vec<ModuleKey>,
    cycle: &mut Vec<ModuleKey>,
) {
    if visited.contains(key) {
        return;
    }
    if in_progress.contains(key) {
        cycle.push(key.clone());
        return;
    }
    if !present.contains(key) {
        return;
    }

    in_progress.insert(key.clone());

    if let Some(node) = nodes.get(key) {
        for dep in node.imports.iter().chain(node.includes.iter()) {
            let resolved: Option<ModuleKey> = if present.contains(dep) {
                Some(dep.clone())
            } else if dep.revision.is_none() {
                name_to_key.get(dep.name.as_str()).map(|k| (*k).clone())
            } else {
                None
            };
            if let Some(resolved_dep) = resolved {
                dfs_visit(
                    &resolved_dep,
                    nodes,
                    name_to_key,
                    present,
                    visited,
                    in_progress,
                    order,
                    cycle,
                );
            }
        }
    }

    in_progress.remove(key);
    visited.insert(key.clone());
    order.push(key.clone());
}

fn extract_deps(key: &ModuleKey, stmt: &Stmt, _errors: &mut Vec<YError>) -> DepNode {
    let mut imports = Vec::new();
    let mut includes = Vec::new();
    let deviates = Vec::new();

    for sub in &stmt.substmts {
        match &sub.keyword {
            k if k.is_builtin(BuiltInKeyword::Import) => {
                let name = sub.arg_str().to_string();
                let rev = sub
                    .get_substmt(BuiltInKeyword::RevisionDate)
                    .map(|s| s.arg_str().to_string());
                imports.push(ModuleKey::new(name, rev));
            }
            k if k.is_builtin(BuiltInKeyword::Include) => {
                let name = sub.arg_str().to_string();
                let rev = sub
                    .get_substmt(BuiltInKeyword::RevisionDate)
                    .map(|s| s.arg_str().to_string());
                includes.push(ModuleKey::new(name, rev));
            }
            k if k.is_builtin(BuiltInKeyword::Deviation) => {
                // deviation "/prefix:node/..." — extract the module prefix
                // to record which module is deviated (resolved later).
                // Store module name derivation at DEVINDEX phase.
                // Here we just note that this module *has* deviations.
                // We don't resolve them to module keys yet (need prefix map).
            }
            _ => {}
        }
    }

    DepNode {
        key: key.clone(),
        imports,
        includes,
        deviates,
        augmented_by: Vec::new(),
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Keyword, Stmt};
    use std::sync::Arc;

    fn make_module(name: &str, imports: &[&str]) -> (ModuleKey, Stmt) {
        let file = Arc::from("test.yang");
        let pos = crate::ast::Pos::new(Arc::clone(&file), 1);
        let subs: Vec<Stmt> = imports
            .iter()
            .map(|dep| {
                let prefix_sub = Stmt::new(
                    Keyword::BuiltIn(BuiltInKeyword::Prefix),
                    Some(dep.to_string()),
                    pos.clone(),
                    vec![],
                );
                Stmt::new(
                    Keyword::BuiltIn(BuiltInKeyword::Import),
                    Some(dep.to_string()),
                    pos.clone(),
                    vec![prefix_sub],
                )
            })
            .collect();

        let stmt = Stmt::new(
            Keyword::BuiltIn(BuiltInKeyword::Module),
            Some(name.to_string()),
            pos,
            subs,
        );
        (ModuleKey::latest(name), stmt)
    }

    #[test]
    fn topo_sort_simple_chain() {
        let modules = vec![
            make_module("c", &["b"]),
            make_module("b", &["a"]),
            make_module("a", &[]),
        ];
        let mut errors = vec![];
        let graph = DepGraph::build(&modules, &mut errors);
        let order = graph.topo_sort().expect("no cycle");
        // a must come before b, b before c
        let ai = order.iter().position(|k| k.name == "a").unwrap();
        let bi = order.iter().position(|k| k.name == "b").unwrap();
        let ci = order.iter().position(|k| k.name == "c").unwrap();
        assert!(ai < bi && bi < ci);
    }

    #[test]
    fn topo_sort_independent_modules() {
        let modules = vec![
            make_module("a", &[]),
            make_module("b", &[]),
            make_module("c", &[]),
        ];
        let mut errors = vec![];
        let graph = DepGraph::build(&modules, &mut errors);
        let order = graph.topo_sort().expect("no cycle");
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn topo_sort_levels_groups_parallel_waves() {
        let modules = vec![
            make_module("d", &["b", "c"]),
            make_module("c", &["a"]),
            make_module("b", &["a"]),
            make_module("a", &[]),
        ];
        let mut errors = vec![];
        let graph = DepGraph::build(&modules, &mut errors);
        let waves = graph.topo_sort_levels().expect("no cycle");

        assert_eq!(waves.len(), 3);
        assert_eq!(waves[0], vec![ModuleKey::latest("a")]);
        assert_eq!(
            waves[1],
            vec![ModuleKey::latest("b"), ModuleKey::latest("c")]
        );
        assert_eq!(waves[2], vec![ModuleKey::latest("d")]);
    }
}
