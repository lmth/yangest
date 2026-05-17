// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Annotation index construction (ANNINDEX phase).
//!
//! Scans the raw `Stmt` trees of overlay modules for extension statements that
//! match registered [`OverlayExtension`] entries and groups the resulting
//! [`PendingAnnotation`] values by the name of the module they target.
//!
//! This mirrors [`crate::devindex`] but handles plugin-declared annotation
//! extensions rather than the built-in `deviation` keyword.

use std::collections::HashMap;

use crate::ast::{BuiltInKeyword, Keyword, ModuleKey, Pos, Stmt};
use crate::compiler::ExtensionInstance;
use crate::plugin::OverlayExtension;

// ── Public types ─────────────────────────────────────────────────────────────

/// An annotation statement found in an overlay module, waiting to be applied
/// to its target schema node during the COMPILE/EXPAND phase.
#[derive(Debug, Clone)]
pub struct PendingAnnotation {
    /// The overlay module that declared this annotation.
    pub from_module: ModuleKey,
    /// Raw argument of the annotation extension statement (the target path,
    /// e.g. `"/ifs:interfaces/ifs:interface"`).
    pub target_path: String,
    /// Prefix of the first path step, if any.  Used to route this annotation
    /// to the correct target module during compilation.
    pub target_prefix: Option<String>,
    /// Resolved name of the module that owns the target node.  `None` when
    /// the path has no prefix (the annotation targets the annotating module
    /// itself).
    pub target_module_name: Option<String>,
    /// Prefix→module-name map from the annotating module's import statements.
    pub prefix_map: HashMap<String, String>,
    /// Source position for diagnostics.
    pub pos: Pos,
    /// Pre-resolved extension instances from the annotation body
    /// (sub-statements of the annotation extension).  These are the
    /// `ExtensionInstance` values that will be injected into the target node
    /// at expansion time.
    pub instances: Vec<ExtensionInstance>,
    /// Name of the plugin that registered the corresponding
    /// [`OverlayExtension`].
    pub source_plugin: &'static str,
}

/// Maps overlay modules to their pending annotations, keyed by the name of
/// the module that owns the annotated nodes.
///
/// Built once in `main.rs` after all overlay modules are parsed; passed
/// read-only to every `compile_module` call.
#[derive(Debug, Default)]
pub struct AnnotationIndex {
    /// `module_name → [PendingAnnotation]` for that module's nodes.
    pub by_target_module: HashMap<String, Vec<PendingAnnotation>>,
}

impl AnnotationIndex {
    /// Build an [`AnnotationIndex`] from a slice of `(ModuleKey, Stmt)` pairs.
    ///
    /// Only entries whose `ModuleKey` appears in `overlay_module_keys` are
    /// scanned; pass `modules` directly when all entries are overlay modules.
    pub fn build(modules: &[(ModuleKey, Stmt)], overlay_exts: &[OverlayExtension]) -> Self {
        if overlay_exts.is_empty() {
            return Self::default();
        }
        let mut index = AnnotationIndex::default();
        for (key, stmt) in modules {
            let anns = extract_module_annotations(key, stmt, overlay_exts);
            for ann in anns {
                let target = match &ann.target_module_name {
                    Some(name) => name.clone(),
                    // No prefix → annotation targets the annotating module itself.
                    None => ann.from_module.name.clone(),
                };
                index.by_target_module.entry(target).or_default().push(ann);
            }
        }
        index
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Returns the prefix→module-name map built from `import` sub-statements.
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

/// Returns the module's own prefix (value of the top-level `prefix` statement).
fn own_prefix(stmt: &Stmt) -> String {
    stmt.get_substmt(BuiltInKeyword::Prefix)
        .and_then(|p| p.arg.clone())
        .unwrap_or_default()
}

/// Extracts all annotation statements from a single overlay module.
fn extract_module_annotations(
    key: &ModuleKey,
    stmt: &Stmt,
    overlay_exts: &[OverlayExtension],
) -> Vec<PendingAnnotation> {
    let prefix_map = raw_prefix_map(stmt);
    let own_pfx = own_prefix(stmt);
    let mut result = Vec::new();
    scan_stmts(key, stmt, &prefix_map, &own_pfx, overlay_exts, &mut result);
    result
}

/// Recursively scan all statements for matching extension occurrences.
fn scan_stmts(
    key: &ModuleKey,
    stmt: &Stmt,
    prefix_map: &HashMap<String, String>,
    own_pfx: &str,
    overlay_exts: &[OverlayExtension],
    out: &mut Vec<PendingAnnotation>,
) {
    for sub in &stmt.substmts {
        let (ext_module, ext_name) = match &sub.keyword {
            Keyword::ExtensionPrefixed { prefix, name } => {
                let module = if prefix == own_pfx {
                    key.name.clone()
                } else {
                    match prefix_map.get(prefix.as_str()) {
                        Some(m) => m.clone(),
                        None => continue,
                    }
                };
                (module, name.clone())
            }
            Keyword::Extension { module, name } => (module.clone(), name.clone()),
            _ => {
                // Recurse into non-extension statements (e.g. `container` in a
                // groupings-only annotation module), though in practice annotation
                // modules are flat.
                scan_stmts(key, sub, prefix_map, own_pfx, overlay_exts, out);
                continue;
            }
        };

        let Some(overlay_ext) = overlay_exts
            .iter()
            .find(|e| e.module == ext_module && e.name == ext_name)
        else {
            continue;
        };

        let target_path = match &sub.arg {
            Some(a) => a.clone(),
            None => continue,
        };
        if target_path.is_empty() {
            continue;
        }

        let target_prefix = extract_first_prefix(&target_path);
        let target_module_name = target_prefix
            .as_deref()
            .and_then(|pfx| prefix_map.get(pfx))
            .cloned();

        let instances =
            resolve_ext_instances(&sub.substmts, own_pfx, &key.name, prefix_map);

        out.push(PendingAnnotation {
            from_module: key.clone(),
            target_path,
            target_prefix,
            target_module_name,
            prefix_map: prefix_map.clone(),
            pos: sub.pos.clone(),
            instances,
            source_plugin: overlay_ext.source_plugin,
        });
    }
}

/// Resolves the body sub-statements of an annotation into [`ExtensionInstance`]
/// values.  Non-extension sub-statements are silently ignored.
fn resolve_ext_instances(
    body_stmts: &[Stmt],
    own_pfx: &str,
    own_module_name: &str,
    prefix_map: &HashMap<String, String>,
) -> Vec<ExtensionInstance> {
    body_stmts
        .iter()
        .filter_map(|sub| {
            let (module, name) = match &sub.keyword {
                Keyword::ExtensionPrefixed { prefix, name } => {
                    let module = if prefix == own_pfx {
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
                _ => return None,
            };
            Some(ExtensionInstance {
                module,
                name,
                arg: sub.arg.clone(),
                substmts: sub.substmts.clone(),
            })
        })
        .collect()
}

/// Returns the prefix of the first path step, if any.
///
/// `"/ifs:interfaces/ifs:interface"` → `Some("ifs")`
/// `"/interfaces/interface"` → `None`
/// Returns the prefix of the deepest (last) prefix-qualified path step, if any.
///
/// Using the deepest prefix correctly identifies the target module even when the path
/// crosses module boundaries, e.g. `/mod-a:container/mod-b:leaf/...`
/// targets `mod-b`, not `mod-a`.
fn extract_first_prefix(path: &str) -> Option<String> {
    path.trim_start_matches('/')
        .split('/')
        .filter_map(|step| step.find(':').map(|colon| step[..colon].to_string()))
        .last()
}
