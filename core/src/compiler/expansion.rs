// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
use std::any::TypeId;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::compile::{expand_children, find_child_in_raw};
use super::{
    CompiledModule, Grouping, IfFeatureExpr, ModuleRegistry, NodeOverlayMap, PathStep, SchemaNode,
    SchemaNodeKind, SchemaPath, Status,
};

#[derive(Clone)]
struct NodePathMarker(pub SchemaPath);

pub(crate) fn attach_schema_path(node: &mut SchemaNode, path: SchemaPath) {
    node.pmap.insert(
        TypeId::of::<NodePathMarker>(),
        Box::new(NodePathMarker(path)),
    );
}

/// Context carried through lazy `uses` expansion.
pub struct ExpansionCtx<'a> {
    /// Enabled features as (module_name, feature_name) pairs.
    /// When empty AND `global_features` is also empty, ALL features are considered enabled.
    pub enabled_features: &'a HashSet<(String, String)>,
    /// Globally-enabled features: a bare feature name that enables the feature in every module
    /// that declares it.  When both this and `enabled_features` are empty, all features are
    /// considered enabled (default/yanger-compatible behaviour).
    pub global_features: &'a HashSet<String>,
    pub registry: &'a ModuleRegistry,
    /// If Some, prune schema nodes whose status exceeds this value at expansion time.
    pub max_status: Option<Status>,
    /// Per-invocation cache: avoids re-expanding the same grouping multiple times within
    /// a single plugin call. Keyed by (grouping raw pointer, using-module own_prefix).
    /// Values are Arc so cache hits are O(1) clones rather than O(n) Vec clones.
    cache: RefCell<HashMap<(*const Grouping, String), Arc<Vec<SchemaNode>>>>,
    /// True when at least one module in the registry has a non-empty overlay
    /// (deviations targeting nodes inside `uses` expansions).
    /// When false, `expand_children` skips all per-node path allocation.
    pub has_any_overlay: bool,
    /// When true, modules that have NO entries in `enabled_features` are treated as
    /// unrestricted (all their features enabled).  This supports bundle mode where
    /// `enabled_features` lists only modules that need restriction; all others are
    /// implicitly unrestricted.
    /// When false (default / CLI `--feature` mode), `enabled_features` is a strict
    /// global whitelist.
    pub unlisted_modules_enabled: bool,
}

impl<'a> ExpansionCtx<'a> {
    pub fn new(
        registry: &'a ModuleRegistry,
        enabled_features: &'a HashSet<(String, String)>,
    ) -> Self {
        static EMPTY_GLOBAL: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
        ExpansionCtx {
            enabled_features,
            global_features: EMPTY_GLOBAL.get_or_init(HashSet::new),
            registry,
            max_status: None,
            cache: RefCell::new(HashMap::new()),
            has_any_overlay: registry.has_any_overlay(),
            unlisted_modules_enabled: false,
        }
    }

    pub fn with_max_status(mut self, max_status: Status) -> Self {
        self.max_status = Some(max_status);
        self
    }

    /// Enable features by bare name across all modules.
    pub fn with_global_features(mut self, global_features: &'a HashSet<String>) -> Self {
        self.global_features = global_features;
        self
    }

    /// Enable the "unlisted modules are unrestricted" mode (bundle semantics).
    ///
    /// In this mode, modules that have no entries in `enabled_features` are treated
    /// as unrestricted (all their features enabled), rather than as fully disabled.
    pub fn with_unlisted_modules_enabled(mut self) -> Self {
        self.unlisted_modules_enabled = true;
        self
    }

    /// Convenience: all features enabled (no filtering).
    pub fn all_features(registry: &'a ModuleRegistry) -> Self {
        static EMPTY: std::sync::OnceLock<HashSet<(String, String)>> = std::sync::OnceLock::new();
        Self::new(registry, EMPTY.get_or_init(HashSet::new))
    }

    /// Returns true when the named feature is enabled.
    ///
    /// Both sets empty ⟹ all features enabled.  Non-empty ⟹ behaviour depends on mode:
    ///
    /// - Default (CLI mode, `unlisted_modules_enabled = false`): strict whitelist.
    ///   A feature is enabled only when its `(module, name)` pair is in `enabled_features`
    ///   OR its name is in `global_features`.
    ///
    /// - Bundle mode (`unlisted_modules_enabled = true`): partial restriction.
    ///   Modules that have NO entries in `enabled_features` are unrestricted (all features
    ///   enabled).  Only modules that appear in `enabled_features` are restricted to their
    ///   listed features.
    pub fn feature_enabled(&self, module_name: &str, feature_name: &str) -> bool {
        if self.enabled_features.is_empty() && self.global_features.is_empty() {
            return true;
        }
        if self.unlisted_modules_enabled {
            // Bundle mode: modules with no entries in enabled_features are unrestricted.
            if !self.enabled_features.iter().any(|(m, _)| m == module_name) {
                return true;
            }
        }
        if self.global_features.contains(feature_name) {
            return true;
        }
        self.enabled_features
            .contains(&(module_name.to_string(), feature_name.to_string()))
    }

    /// Evaluate an `if-feature` expression.
    /// `own_module` is the name of the module that owns the node being evaluated.
    pub fn eval_if_feature(&self, expr: &IfFeatureExpr, own_module: &str) -> bool {
        match expr {
            IfFeatureExpr::Name(feat, None) => self.feature_enabled(own_module, feat),
            IfFeatureExpr::Name(feat, Some(module_name)) => {
                if module_name.is_empty() {
                    return false;
                }
                if self.enabled_features.is_empty() && self.global_features.is_empty() {
                    return true;
                }
                self.feature_enabled(module_name, feat)
            }
            IfFeatureExpr::Not(inner) => !self.eval_if_feature(inner, own_module),
            IfFeatureExpr::And(a, b) => {
                self.eval_if_feature(a, own_module) && self.eval_if_feature(b, own_module)
            }
            IfFeatureExpr::Or(a, b) => {
                self.eval_if_feature(a, own_module) || self.eval_if_feature(b, own_module)
            }
        }
    }

    pub(crate) fn cache_get(
        &self,
        grouping_ptr: *const Grouping,
        own_prefix: &str,
    ) -> Option<Arc<Vec<SchemaNode>>> {
        self.cache
            .borrow()
            .get(&(grouping_ptr, own_prefix.to_string()))
            .map(Arc::clone)
    }

    pub(crate) fn cache_insert(
        &self,
        grouping_ptr: *const Grouping,
        own_prefix: &str,
        nodes: Arc<Vec<SchemaNode>>,
    ) {
        self.cache
            .borrow_mut()
            .insert((grouping_ptr, own_prefix.to_string()), nodes);
    }
}

impl CompiledModule {
    pub fn children(&self, ctx: &ExpansionCtx<'_>) -> Vec<SchemaNode> {
        // Fast path: no deviation overlay on this module — skip path tracking.
        if self.overlay.is_empty() {
            let empty_overlay = NodeOverlayMap::new();
            return expand_children(
                &self.children,
                &self.prefix,
                &self.key.name,
                &empty_overlay,
                &[],
                ctx,
            );
        }
        expand_children(
            &self.children,
            &self.prefix,
            &self.key.name,
            &self.overlay,
            &[],
            ctx,
        )
    }

    /// Find a named top-level child with early termination, expanding Uses as needed.
    ///
    /// Unlike [`children`](Self::children) which expands all children, this stops as
    /// soon as the target name is found.  Use for step-by-step path navigation.
    pub fn find_child(&self, name: &str, ctx: &ExpansionCtx<'_>) -> Option<SchemaNode> {
        find_child_in_raw(name, &self.children, &self.overlay, ctx)
    }
}

impl SchemaNode {
    pub fn schema_path(&self) -> Option<SchemaPath> {
        self.pmap
            .get(&TypeId::of::<NodePathMarker>())
            .and_then(|value| value.downcast_ref::<NodePathMarker>())
            .map(|marker| marker.0.clone())
    }

    pub fn children(&self, ctx: &ExpansionCtx<'_>) -> Vec<SchemaNode> {
        let Some(raw) = self.raw_children() else {
            return Vec::new();
        };
        if !ctx.has_any_overlay {
            // Fast path: no overlay anywhere — skip registry lookup and path tracking.
            let empty_overlay = NodeOverlayMap::new();
            return expand_children(
                raw,
                &self.module_prefix,
                &self.module_name,
                &empty_overlay,
                &[],
                ctx,
            );
        }
        let empty_overlay = NodeOverlayMap::new();
        let overlay_owner = ctx.registry.resolve_import(&self.module_name, None);
        let overlay = overlay_owner
            .as_ref()
            .map(|module| &module.overlay)
            .unwrap_or(&empty_overlay);
        let node_path = self.schema_path().unwrap_or_else(|| {
            vec![PathStep {
                prefix: Some(self.module_prefix.clone()),
                name: self.name.clone(),
            }]
        });
        expand_children(
            raw,
            &self.module_prefix,
            &self.module_name,
            overlay,
            &node_path,
            ctx,
        )
    }

    pub fn input_children(&self, ctx: &ExpansionCtx<'_>) -> Vec<SchemaNode> {
        let raw = match &self.kind {
            SchemaNodeKind::Rpc { input, .. } | SchemaNodeKind::Action { input, .. } => input,
            _ => return Vec::new(),
        };
        self.expand_named_io(raw, "input", ctx)
    }

    pub fn output_children(&self, ctx: &ExpansionCtx<'_>) -> Vec<SchemaNode> {
        let raw = match &self.kind {
            SchemaNodeKind::Rpc { output, .. } | SchemaNodeKind::Action { output, .. } => output,
            _ => return Vec::new(),
        };
        self.expand_named_io(raw, "output", ctx)
    }

    pub fn raw_children(&self) -> Option<&[SchemaNode]> {
        match &self.kind {
            SchemaNodeKind::Container { children, .. }
            | SchemaNodeKind::List { children, .. }
            | SchemaNodeKind::Case { children }
            | SchemaNodeKind::Notification { children, .. } => Some(children),
            SchemaNodeKind::Choice { cases, .. } => Some(cases),
            _ => None,
        }
    }

    /// Find a named child with early termination, expanding Uses groupings as needed.
    ///
    /// Unlike [`children`](Self::children) which expands all children into a Vec, this
    /// stops as soon as the target name is found.  Use for step-by-step path navigation
    /// where only a single child is needed per level.
    pub fn find_child(&self, name: &str, ctx: &ExpansionCtx<'_>) -> Option<SchemaNode> {
        let raw = self.raw_children()?;
        let empty_overlay = NodeOverlayMap::new();
        if !ctx.has_any_overlay {
            return find_child_in_raw(name, raw, &empty_overlay, ctx);
        }
        let overlay_owner = ctx.registry.resolve_import(&self.module_name, None);
        let overlay = overlay_owner
            .as_ref()
            .map(|m| &m.overlay)
            .unwrap_or(&empty_overlay);
        find_child_in_raw(name, raw, overlay, ctx)
    }

    /// Find a named child in the RPC/action input, expanding Uses as needed.
    pub fn find_input_child(&self, name: &str, ctx: &ExpansionCtx<'_>) -> Option<SchemaNode> {
        let raw = match &self.kind {
            SchemaNodeKind::Rpc { input, .. } | SchemaNodeKind::Action { input, .. } => input,
            _ => return None,
        };
        let empty_overlay = NodeOverlayMap::new();
        find_child_in_raw(name, raw, &empty_overlay, ctx)
    }

    /// Find a named child in the RPC/action output, expanding Uses as needed.
    pub fn find_output_child(&self, name: &str, ctx: &ExpansionCtx<'_>) -> Option<SchemaNode> {
        let raw = match &self.kind {
            SchemaNodeKind::Rpc { output, .. } | SchemaNodeKind::Action { output, .. } => output,
            _ => return None,
        };
        let empty_overlay = NodeOverlayMap::new();
        find_child_in_raw(name, raw, &empty_overlay, ctx)
    }

    fn expand_named_io(
        &self,
        raw: &[SchemaNode],
        name: &str,
        ctx: &ExpansionCtx<'_>,
    ) -> Vec<SchemaNode> {
        if !ctx.has_any_overlay {
            // Fast path: skip registry lookup and path tracking.
            let empty_overlay = NodeOverlayMap::new();
            return expand_children(
                raw,
                &self.module_prefix,
                &self.module_name,
                &empty_overlay,
                &[],
                ctx,
            );
        }
        let empty_overlay = NodeOverlayMap::new();
        let overlay_owner = ctx.registry.resolve_import(&self.module_name, None);
        let overlay = overlay_owner
            .as_ref()
            .map(|module| &module.overlay)
            .unwrap_or(&empty_overlay);
        let mut node_path = self.schema_path().unwrap_or_else(|| {
            vec![PathStep {
                prefix: Some(self.module_prefix.clone()),
                name: self.name.clone(),
            }]
        });
        node_path.push(PathStep {
            prefix: Some(self.module_prefix.clone()),
            name: name.to_string(),
        });
        expand_children(
            raw,
            &self.module_prefix,
            &self.module_name,
            overlay,
            &node_path,
            ctx,
        )
    }

    pub fn is_rpc(&self) -> bool {
        matches!(self.kind, SchemaNodeKind::Rpc { .. })
    }

    pub fn is_notification(&self) -> bool {
        matches!(self.kind, SchemaNodeKind::Notification { .. })
    }

    pub fn is_action(&self) -> bool {
        matches!(self.kind, SchemaNodeKind::Action { .. })
    }

    pub fn is_enabled(&self, ctx: &ExpansionCtx<'_>) -> bool {
        if let Some(max_status) = ctx.max_status {
            if self.status > max_status {
                return false;
            }
        }
        self.if_features
            .iter()
            .all(|expr| ctx.eval_if_feature(expr, &self.module_name))
    }
}
