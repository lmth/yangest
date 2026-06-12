// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
use std::any::TypeId;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use super::compile::{expand_children, expand_children_all, expand_children_and_all, expand_children_with_secondary, find_child_in_raw, mark_augment_injected};
use super::{
    AugmentEntry, CompiledModule, Grouping, IfFeatureExpr, ModuleRegistry, NodeOverlayMap, PathStep,
    SchemaNode, SchemaNodeKind, SchemaPath, Status,
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
    /// considered enabled (the default behaviour).
    pub global_features: &'a HashSet<String>,
    pub registry: &'a ModuleRegistry,
    /// If Some, prune schema nodes whose status exceeds this value at expansion time.
    pub max_status: Option<Status>,
    /// Per-invocation cache: avoids re-expanding the same grouping multiple times within
    /// a single plugin call. Keyed by (grouping raw pointer, using-module own_prefix).
    /// Values are Arc so cache hits are O(1) clones rather than O(n) Vec clones.
    cache: RefCell<HashMap<(*const Grouping, String), Arc<Vec<SchemaNode>>>>,
    /// Optional shared cross-module expand cache. When set, `cache_get` checks this cache
    /// AFTER the per-invocation cache, and `cache_insert` also writes to it.
    /// Keyed by (grouping address as usize, own_prefix).
    /// This allows all module emits in a parallel bundle to share expansion results.
    pub shared_expand_cache: Option<Arc<RwLock<HashMap<(usize, String), Arc<Vec<SchemaNode>>>>>>,
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
    /// The overlay of the "file module" currently being compiled/emitted.
    ///
    /// When a node belongs to a grouping from an external module (its own module's overlay
    /// is empty), `SchemaNode::children()` uses this field instead so that deviations
    /// targeting nodes inside external groupings are still applied correctly.
    ///
    /// Stored as a raw pointer to avoid making `ExpansionCtx<'a>` invariant over `'a`
    /// (which `Cell<Option<&'a T>>` would cause).  The pointer is only valid during the
    /// current module emission call and is never stored beyond that.
    pub file_module_overlay: Cell<*const NodeOverlayMap>,
    /// Holds Arc<Grouping> references for all groupings that have been inserted into
    /// the expansion cache. This prevents the Arcs from being deallocated (and their
    /// memory addresses reused), which would corrupt the pointer-based cache keys.
    cache_keepalive: RefCell<Vec<Arc<Grouping>>>,
    /// Cache of *expanded* augment bodies, keyed by `(AugmentEntry ptr, own_prefix)`.
    /// A `uses g;` augment body stores a single `Uses` node in `aug.nodes`; the
    /// cursor must expand it so the grouping's leaves are visible. The
    /// `AugmentEntry` lives in a registry-held `CompiledModule` that outlives the
    /// ctx, so the pointer key is stable.
    augment_cache: RefCell<HashMap<(*const AugmentEntry, String), Arc<Vec<SchemaNode>>>>,
    /// Optional sink for plugin-raised diagnostics. The host wires this in before
    /// emit (see [`with_diagnostics`](Self::with_diagnostics)); plugins reach it
    /// via [`diagnostics`](Self::diagnostics). `None` on the compile path.
    diagnostics: Option<&'a crate::diagnostics::Diagnostics>,
    /// Optional AST-annotation index, needed only when
    /// [`CompilationFlags::scope_grouping_annotations_to_target`] is set, to scope
    /// annotation-injected `must`/`when` constraints to their target module during
    /// `uses` expansion.
    annotation_index: Option<&'a crate::astannindex::AstAnnotationIndex>,
    /// Memoised annotation-scope verdicts, used only under
    /// `scope_grouping_annotations_to_target`. Keyed by grouping pointer (the
    /// annotation content of a grouping body is intrinsic to the grouping, so it
    /// does not depend on the using prefix). Avoids re-walking a grouping's whole
    /// expansion on every `expand_uses_lazy` call — see `GroupingScopeVerdict`.
    pub(crate) ann_scope_cache: RefCell<HashMap<usize, GroupingScopeVerdict>>,
}

/// Cached annotation-scope verdict for one grouping (keyed by its pointer in
/// [`ExpansionCtx::ann_scope_cache`]).
#[derive(Default)]
pub(crate) struct GroupingScopeVerdict {
    /// Whether the grouping's expansion contains *any* annotation-injected
    /// `must`/`when`/extension (target-independent). When false, no consuming
    /// module ever needs filtering — the expansion is returned untouched.
    pub(crate) has_any: bool,
    /// Per-consuming-module memo: whether a *foreign* annotation-injected
    /// statement is present (so the expansion must be cloned and filtered).
    pub(crate) foreign: HashMap<String, bool>,
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
            shared_expand_cache: None,
            has_any_overlay: registry.has_any_overlay(),
            unlisted_modules_enabled: false,
            file_module_overlay: Cell::new(std::ptr::null()),
            cache_keepalive: RefCell::new(Vec::new()),
            augment_cache: RefCell::new(HashMap::new()),
            diagnostics: None,
            annotation_index: None,
            ann_scope_cache: RefCell::new(HashMap::new()),
        }
    }

    /// Attach a plugin-diagnostics sink. Called by the host before emit so that
    /// plugins can raise warnings/errors via [`diagnostics`](Self::diagnostics).
    pub fn with_diagnostics(mut self, diagnostics: &'a crate::diagnostics::Diagnostics) -> Self {
        self.diagnostics = Some(diagnostics);
        self
    }

    /// The plugin-diagnostics sink, if the host attached one. A plugin raises a
    /// diagnostic with e.g. `ctx.diagnostics().map(|d| d.warning(self.name(), msg))`.
    pub fn diagnostics(&self) -> Option<&'a crate::diagnostics::Diagnostics> {
        self.diagnostics
    }

    /// Attach the AST-annotation index, enabling
    /// [`CompilationFlags::scope_grouping_annotations_to_target`](crate::compiler::CompilationFlags)
    /// scoping during `uses` expansion. Harmless when that flag is off.
    pub fn with_annotation_index(
        mut self,
        index: &'a crate::astannindex::AstAnnotationIndex,
    ) -> Self {
        self.annotation_index = Some(index);
        self
    }

    /// The AST-annotation index, if attached.
    pub fn annotation_index(&self) -> Option<&'a crate::astannindex::AstAnnotationIndex> {
        self.annotation_index
    }

    /// Return the *expanded* children contributed by an augment body, caching the
    /// result keyed on the augment.
    ///
    /// The augmenting `module`'s own overlay is applied with the augment's target
    /// path as the parent path, so target-path-keyed annotations (e.g. a `when` or
    /// `must` that an annotation module attaches several levels into the augment
    /// body) land on the composed view. This is the same expansion the tree plugin
    /// and compile path perform for an augment section
    /// (`expand_children(&aug.nodes, &module.prefix, &module.key.name,
    /// &module.overlay, &aug.target_path, ctx)`); composing the body without the
    /// overlay would silently drop those annotations.
    ///
    /// Exposed (`pub`) so a backend can obtain the composed body of a *single*
    /// augment with provenance — see
    /// [`Cursor::augments_at`](crate::cursor::Cursor::augments_at).
    pub fn expand_augment_body(
        &self,
        module: &CompiledModule,
        aug: &AugmentEntry,
    ) -> Arc<Vec<SchemaNode>> {
        let key = (aug as *const AugmentEntry, module.prefix.clone());
        if let Some(cached) = self.augment_cache.borrow().get(&key) {
            return Arc::clone(cached);
        }
        let mut expanded = expand_children(
            &aug.nodes,
            &module.prefix,
            &module.key.name,
            &module.overlay,
            &aug.target_path,
            self,
        );
        // Flag the whole grafted subtree as augment-injected — including nodes a
        // `uses` inside the augment contributed (the reference's
        // `set_module_name_and_config` rewrites those too). `expand_children`
        // returns freshly-cloned nodes and `mark_augment_injected` clones any
        // still-shared children before mutating, so the grouping cache is
        // untouched; the stamp is amortised by the `augment_cache` below.
        mark_augment_injected(&mut expanded);
        let arc = Arc::new(expanded);
        self.augment_cache.borrow_mut().insert(key, Arc::clone(&arc));
        arc
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

    /// Attach a shared cross-module expand cache. When set, `cache_get` checks this global
    /// cache after the per-invocation cache, and `cache_insert` also populates it.
    pub fn with_shared_expand_cache(
        mut self,
        cache: Arc<RwLock<HashMap<(usize, String), Arc<Vec<SchemaNode>>>>>,
    ) -> Self {
        self.shared_expand_cache = Some(cache);
        self
    }

    /// Set the file module overlay for this context.
    ///
    /// This overlay is used by `SchemaNode::children()` as a fallback when a node's own
    /// module has no overlay.  This handles nodes from external groupings (expanded via
    /// `uses`) whose `module_name` points to the source module rather than the file module.
    pub fn with_file_module_overlay(self, overlay: &NodeOverlayMap) -> Self {
        self.file_module_overlay.set(overlay as *const _);
        self
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
        // Check per-invocation cache first (no lock needed).
        if let Some(arc) = self
            .cache
            .borrow()
            .get(&(grouping_ptr, own_prefix.to_string()))
            .map(Arc::clone)
        {
            return Some(arc);
        }
        // Check shared global cache (read lock).
        if let Some(ref shared) = self.shared_expand_cache {
            if let Ok(map) = shared.read() {
                if let Some(arc) = map.get(&(grouping_ptr as usize, own_prefix.to_string())) {
                    let arc = Arc::clone(arc);
                    // Populate per-invocation cache so subsequent lookups skip the lock.
                    self.cache
                        .borrow_mut()
                        .insert((grouping_ptr, own_prefix.to_string()), Arc::clone(&arc));
                    return Some(arc);
                }
            }
        }
        None
    }

    pub(crate) fn cache_insert(
        &self,
        grouping_ptr: *const Grouping,
        own_prefix: &str,
        nodes: Arc<Vec<SchemaNode>>,
        grouping_arc: &Arc<Grouping>,
    ) {
        self.cache_keepalive.borrow_mut().push(Arc::clone(grouping_arc));
        self.cache
            .borrow_mut()
            .insert((grouping_ptr, own_prefix.to_string()), Arc::clone(&nodes));
        // Also populate shared global cache if present.
        if let Some(ref shared) = self.shared_expand_cache {
            if let Ok(mut map) = shared.write() {
                map.entry((grouping_ptr as usize, own_prefix.to_string()))
                    .or_insert(nodes);
            }
        }
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

    /// Like [`children`](Self::children) but includes feature-gated nodes.  Used for
    /// collecting inline enum types from the full schema tree (all nodes are
    /// walked regardless of if-feature state).
    pub fn all_children(&self, ctx: &ExpansionCtx<'_>) -> Vec<SchemaNode> {
        let empty_overlay = NodeOverlayMap::new();
        let overlay = if self.overlay.is_empty() {
            &empty_overlay
        } else {
            &self.overlay
        };
        // Use expand_children_and_all (not expand_children_all) so that:
        // 1. Schema paths are attached to returned nodes (needed for correct deviation
        //    lookup when SchemaNode::all_children() recurses into children).
        // 2. `deviate not-supported` overlays at the top level are applied correctly.
        let (_enabled, all) = expand_children_and_all(
            &self.children,
            &self.prefix,
            &self.key.name,
            overlay,
            &[],
            ctx,
        );
        all
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

        // Get file_module_overlay (annotations from the emitting module targeting nodes
        // in external groupings).
        let fallback_ptr = ctx.file_module_overlay.get();
        let file_overlay = if fallback_ptr.is_null() {
            &empty_overlay
        } else {
            // SAFETY: The pointer is set by `walk_module` immediately before calling
            // `module.children(ctx)` and points to the module's overlay, which is owned
            // by the `CompiledModule` that outlives this call.
            unsafe { &*fallback_ptr }
        };

        // Both overlays empty → fast path.
        if overlay.is_empty() && file_overlay.is_empty() {
            return expand_children(
                raw,
                &self.module_prefix,
                &self.module_name,
                &empty_overlay,
                &[],
                ctx,
            );
        }

        let node_path = self.schema_path().unwrap_or_else(|| {
            vec![PathStep {
                prefix: Some(self.module_prefix.clone()),
                name: self.name.clone(),
            }]
        });

        // Determine primary/secondary overlay pairing.  When both are non-empty and
        // different, use the node's own module overlay as primary and file_module_overlay
        // as secondary (fallback for keys not found in primary).
        if overlay.is_empty() {
            // Only file overlay — use it as primary.
            expand_children(
                raw,
                &self.module_prefix,
                &self.module_name,
                file_overlay,
                &node_path,
                ctx,
            )
        } else if file_overlay.is_empty() || std::ptr::eq(overlay, file_overlay) {
            // Only node overlay (or same pointer) — use it as primary.
            expand_children(
                raw,
                &self.module_prefix,
                &self.module_name,
                overlay,
                &node_path,
                ctx,
            )
        } else {
            // Both non-empty and different — use secondary overlay for fallback lookups.
            expand_children_with_secondary(
                raw,
                &self.module_prefix,
                &self.module_name,
                overlay,
                file_overlay,
                &node_path,
                ctx,
            )
        }
    }

    /// Like [`children`](Self::children) but includes feature-gated nodes (if-feature
    /// conditions are NOT evaluated). Used for type collection, where the full
    /// schema tree must be walked regardless of if-feature state.
    ///
    /// Unlike the old implementation, this DOES apply `deviate not-supported` overlays
    /// (using the module's overlay map with the node's schema_path for key matching),
    /// so deviated-away nodes are correctly excluded from the result.
    pub fn all_children(&self, ctx: &ExpansionCtx<'_>) -> Vec<SchemaNode> {
        let Some(raw) = self.raw_children() else {
            return Vec::new();
        };
        if !ctx.has_any_overlay {
            // Fast path: no overlay anywhere — no deviations to apply.
            let empty_overlay = NodeOverlayMap::new();
            let (_enabled, all) = expand_children_and_all(
                raw,
                &self.module_prefix,
                &self.module_name,
                &empty_overlay,
                &[],
                ctx,
            );
            return all;
        }
        let empty_overlay = NodeOverlayMap::new();
        let overlay_owner = ctx.registry.resolve_import(&self.module_name, None);
        let overlay = overlay_owner
            .as_ref()
            .map(|module| &module.overlay)
            .unwrap_or(&empty_overlay);

        // Get file_module_overlay for secondary lookup.
        let fallback_ptr = ctx.file_module_overlay.get();
        let file_overlay = if fallback_ptr.is_null() {
            &empty_overlay
        } else {
            unsafe { &*fallback_ptr }
        };

        // Both empty → fast path.
        if overlay.is_empty() && file_overlay.is_empty() {
            let (_enabled, all) = expand_children_and_all(
                raw,
                &self.module_prefix,
                &self.module_name,
                &empty_overlay,
                &[],
                ctx,
            );
            return all;
        }

        let node_path = self.schema_path().unwrap_or_else(|| {
            vec![PathStep {
                prefix: Some(self.module_prefix.clone()),
                name: self.name.clone(),
            }]
        });

        // Use the non-empty overlay (or merge if both are non-empty).
        // For all_children, expand_children_and_all only checks for not-supported
        // deviations so exact merge semantics don't affect type collection.
        let effective = if overlay.is_empty() {
            file_overlay
        } else if file_overlay.is_empty() || std::ptr::eq(overlay, file_overlay) {
            overlay
        } else {
            // Both non-empty: use node's overlay (sufficient for not-supported checks
            // since all_children is used for type collection, not must/when application).
            overlay
        };

        let (_enabled, all) = expand_children_and_all(
            raw,
            &self.module_prefix,
            &self.module_name,
            effective,
            &node_path,
            ctx,
        );
        all
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
