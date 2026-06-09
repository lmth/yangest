// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Canonical schema walker (see `docs/core-walker-design.md`).
//!
//! A backend that needs to reproduce an order-sensitive artefact (e.g. a
//! `ns_to_prefix_maps` list whose content depends on the order in which the
//! reference compiler resolved types and leafrefs) cannot recover that order
//! from a finalised [`CompiledModule`] — the encounter order is only observable
//! *during* traversal.
//!
//! [`SchemaWalker`] is the driver that walks the compiled schema in a canonical
//! order and fires [`TypeResolutionObserver`] events at each type-resolution
//! site, by calling [`TypeRegistry::resolve_with_observer`] /
//! [`TypeRegistry::follow_leafref_with_observer`] at the right moments.
//!
//! ## Visit order (§5 of the design doc)
//!
//! Starting at the module root, children are visited in declaration order (the
//! post-grouping, post-deviation order already stored on the nodes). At each
//! node the node's own type is resolved *before* descending into its children.
//! `choice`/`case` are descended into structurally but are transparent for
//! leafref/cursor purposes (a leaf inside a `case` resolves relative to the
//! enclosing data node).
//!
//! ## Scope
//!
//! This implements the design doc's migration steps 1–2 (skeleton + observer
//! firing), step 3 (own-augment traversal), step 5 (`on_constraint_source` for
//! `when`/`must`), and step 6 (`on_extension_attached` for foreign-module
//! extensions). At each node the walker fires, in order: constraint-source
//! events, foreign-extension events, then type-resolution events; then it
//! descends.
//!
//! [`walk`](SchemaWalker::walk) visits the module's own tree
//! ([`walk_own_tree`](SchemaWalker::walk_own_tree)) and then the augments the
//! module applies into other modules
//! ([`walk_own_augments`](SchemaWalker::walk_own_augments)), so a backend
//! finalising per-module output sees *all* of a module's typed leafs — including
//! those it contributes under other modules' trees — during that module's walk.
//!
//! [`WalkOptions::follow_deviations`] and [`WalkOptions::deep_typedef_chains`]
//! are accepted and stored but reserved for future work (step 4 / open
//! questions).
//!
//! Like the rest of the resolution machinery, the walker is **opt-in**: it is
//! constructed by a backend from what `Plugin::emit_module` already has, and
//! nothing in the compile path uses it.

use crate::compiler::{
    AugmentEntry, CompiledModule, ExpansionCtx, ModuleRegistry, PathStep, SchemaNode,
    SchemaNodeKind, SchemaPath,
};
use crate::cursor::{Axis, Cursor, QName};
use crate::types_registry::{BuiltInType, ConstraintKind, TypeRegistry, TypeResolutionObserver};

/// Tuning knobs for [`SchemaWalker`]. [`WalkOptions::default`] is the
/// reference-compiler-faithful configuration; backends customise only when they
/// explicitly need to.
#[derive(Clone, Copy, Debug)]
pub struct WalkOptions {
    /// Visit deviation/augment subtrees inline at their target position.
    ///
    /// Reserved for the augment-splicing/deviation-inlining work (design-doc
    /// migration step 3); currently the walk visits each module's own
    /// `children(ctx)` and this flag does not yet change behaviour.
    pub follow_deviations: bool,
    /// Fire observer events for typedef chains nested inside an already-resolved
    /// typedef. Default `false` (matches the reference compiler: the
    /// resolved-typedef cache short-circuits repeated chains).
    pub deep_typedef_chains: bool,
}

impl Default for WalkOptions {
    fn default() -> Self {
        // Reference-compiler-faithful defaults. Note: this is a manual impl
        // rather than `#[derive(Default)]` because the faithful value of
        // `follow_deviations` is `true`, not the bool default `false`.
        WalkOptions { follow_deviations: true, deep_typedef_chains: false }
    }
}

/// Drives a schema walk that fires [`TypeResolutionObserver`] events in the
/// reference compiler's encounter order.
pub struct SchemaWalker<'a> {
    module: &'a CompiledModule,
    registry: &'a ModuleRegistry,
    types: &'a TypeRegistry<'a>,
    ctx: &'a ExpansionCtx<'a>,
    options: WalkOptions,
}

impl<'a> SchemaWalker<'a> {
    /// Construct a walker from the pieces a `Plugin::emit_module` already holds.
    pub fn new(
        module: &'a CompiledModule,
        registry: &'a ModuleRegistry,
        types: &'a TypeRegistry<'a>,
        ctx: &'a ExpansionCtx<'a>,
    ) -> Self {
        SchemaWalker { module, registry, types, ctx, options: WalkOptions::default() }
    }

    /// Override the walk options (builder style).
    pub fn with_options(mut self, options: WalkOptions) -> Self {
        self.options = options;
        self
    }

    /// The module being walked.
    pub fn module(&self) -> &'a CompiledModule {
        self.module
    }

    /// The registry the walk resolves against.
    pub fn registry(&self) -> &'a ModuleRegistry {
        self.registry
    }

    /// The active walk options.
    pub fn options(&self) -> WalkOptions {
        self.options
    }

    /// Walk the module's schema tree *and* the augments it applies into other
    /// modules, firing observer events in encounter order: own-tree first, then
    /// own-augments (§11). The observer is borrowed for the duration of the walk
    /// and may carry arbitrary backend state.
    pub fn walk<O: TypeResolutionObserver>(&self, observer: &mut O) {
        self.walk_with_visitor(observer, |_| {});
    }

    /// Like [`walk`](Self::walk) but exposes the cursor at every visit. The
    /// `visit` closure is called *before* type resolution at each node; type
    /// resolution still fires through `observer`.
    pub fn walk_with_visitor<O, V>(&self, observer: &mut O, mut visit: V)
    where
        O: TypeResolutionObserver,
        V: FnMut(&Cursor<'_>),
    {
        self.walk_own_tree_inner(observer, &mut visit);
        self.walk_own_augments_inner(observer, &mut visit);
    }

    /// Walk only the module's *own* schema tree (its `children(ctx)`), not the
    /// augments it applies into other modules.
    pub fn walk_own_tree<O: TypeResolutionObserver>(&self, observer: &mut O) {
        self.walk_own_tree_inner(observer, &mut |_| {});
    }

    /// For every augment this module applies into *another* module's tree, walk
    /// the augment's contributed children — positioned in the host tree so
    /// leafref paths resolve correctly and host-applied annotations are surfaced
    /// — firing observer events as if the children were part of this module.
    ///
    /// A backend finalising per-module output must run this during the
    /// augmenting module's walk, because those typed leafs structurally live
    /// under another module's tree but belong to this module (§11).
    pub fn walk_own_augments<O: TypeResolutionObserver>(&self, observer: &mut O) {
        self.walk_own_augments_inner(observer, &mut |_| {});
    }

    fn walk_own_tree_inner<O, V>(&self, observer: &mut O, visit: &mut V)
    where
        O: TypeResolutionObserver,
        V: FnMut(&Cursor<'_>),
    {
        let root = Cursor::root_of(self.module, self.ctx);
        for child in self.module.children(self.ctx) {
            self.visit_node(&child, &root, observer, visit);
        }
    }

    fn walk_own_augments_inner<O, V>(&self, observer: &mut O, visit: &mut V)
    where
        O: TypeResolutionObserver,
        V: FnMut(&Cursor<'_>),
    {
        for aug in &self.module.augments {
            if !self.augment_is_enabled(aug) {
                continue;
            }
            let Some(target_cursor) = self.cursor_for_augment_target(&aug.target_path) else {
                continue;
            };
            // For each node this augment contributes (declaration order), visit
            // the matching child in the *host's* compiled tree at the target —
            // not the raw `aug.nodes` — so extensions the host's compile applied
            // at the splice point (e.g. annotation-injected ones) are surfaced
            // (§11 host-tree-iteration rule). Matching by name (rather than
            // visiting every same-module child) keeps two augments into the same
            // target from double-counting each other's nodes.
            //
            // NOTE: when an augment body is `uses g;` (so `aug.nodes` is a
            // single `Uses` node named `__uses__`), this name-match loop
            // visits zero nodes — the §11 follow-up. The natural fix (expand
            // both `aug.nodes` and `host_children` via `expand_children`)
            // was attempted in `84da49f` and reverted in `<this commit>`
            // because of catastrophic per-descent allocation costs on heavy
            // bundles; see §11 for the three candidate paths forward.
            // Contributed names come from the *expanded* augment body (a `uses g;`
            // body stores a single `Uses` node), served from the cached expansion
            // the cursor also uses.
            let body = self.ctx.expand_augment_body(self.module, aug);
            let host_children = target_cursor.child_nodes();
            for body_node in body.iter() {
                if let Some(child) = host_children.iter().find(|c| {
                    c.name == body_node.name && c.module_name == self.module.key.name
                }) {
                    self.visit_node(child, &target_cursor, observer, visit);
                }
            }
        }
    }

    /// True if every `if-feature` on the augment is enabled (an augment whose
    /// guard fails contributes no nodes and therefore fires no events).
    fn augment_is_enabled(&self, aug: &AugmentEntry) -> bool {
        aug.if_features
            .iter()
            .all(|f| self.ctx.eval_if_feature(f, &self.module.key.name))
    }

    /// Position a cursor at an augment's target node in the host module. Returns
    /// `None` if the host module or target node cannot be reached (best-effort;
    /// the walk skips such augments rather than failing).
    fn cursor_for_augment_target(&self, target_path: &SchemaPath) -> Option<Cursor<'a>> {
        let first = target_path.first()?;
        let host = self.step_module(first);
        let mut cur = Cursor::root_of(self.module, self.ctx).reroot(&host)?;
        let axes: Vec<Axis> = target_path
            .iter()
            .map(|step| {
                Axis::Child(QName {
                    module: Some(self.step_module(step)),
                    name: step.name.clone(),
                })
            })
            .collect();
        cur.follow_path(&axes).ok()?;
        Some(cur)
    }

    /// Resolve a target-path step's prefix to a module name, in the context of
    /// this (augmenting) module's imports. Unprefixed / own-prefix → this module.
    fn step_module(&self, step: &PathStep) -> String {
        match &step.prefix {
            None => self.module.key.name.clone(),
            Some(p) if *p == self.module.prefix => self.module.key.name.clone(),
            Some(p) => self
                .module
                .prefix_map
                .get(p)
                .cloned()
                .unwrap_or_else(|| self.module.key.name.clone()),
        }
    }

    /// Visit `node`, whose nearest enclosing *data* node is positioned at
    /// `data_cursor`. `choice`/`case` nodes are transparent: they are descended
    /// into structurally but do not advance the cursor, so their leaf descendants
    /// resolve relative to the enclosing data node (matching how leafref paths
    /// treat them).
    fn visit_node<O, V>(
        &self,
        node: &SchemaNode,
        data_cursor: &Cursor<'a>,
        observer: &mut O,
        visit: &mut V,
    ) where
        O: TypeResolutionObserver,
        V: FnMut(&Cursor<'_>),
    {
        let transparent =
            matches!(&node.kind, SchemaNodeKind::Choice { .. } | SchemaNodeKind::Case { .. });

        let node_cursor = if transparent {
            data_cursor.clone()
        } else {
            data_cursor
                .find_child(&QName {
                    module: Some(node.module_name.clone()),
                    name: node.name.clone(),
                })
                // Fall back to the parent position if the node is not reachable
                // through the logical (cursor) tree — e.g. a node only present via
                // a cross-module augment the walk does not yet splice. Resolution
                // still proceeds; only relative-path resolution from this node is
                // approximate in that case.
                .unwrap_or_else(|| data_cursor.clone())
        };

        visit(&node_cursor);
        self.fire_constraint_sources(node, observer);
        self.fire_extension_attached(node, observer);
        self.resolve_node_type(node, &node_cursor, observer);

        for child in node.children(self.ctx) {
            self.visit_node(&child, &node_cursor, observer, visit);
        }
    }

    /// Fire `on_constraint_source` for each `when` and `must` on `node`, in
    /// `when`-then-`must` declaration order. Source modules are read from the
    /// expressions themselves: original constraints carry the node's own
    /// module; deviation- or annotation-injected constraints carry the
    /// injecting module's name.
    fn fire_constraint_sources<O: TypeResolutionObserver>(
        &self,
        node: &SchemaNode,
        observer: &mut O,
    ) {
        for w in &node.when {
            observer.on_constraint_source(&w.source_module, ConstraintKind::When, node);
        }
        for m in node.musts() {
            observer.on_constraint_source(&m.source_module, ConstraintKind::Must, node);
        }
    }

    /// Fire `on_extension_attached` for each *foreign-module* extension instance
    /// on `node`, in `node.extensions` declaration order. Same-module extensions
    /// are filtered out (a backend that needs them reads `node.extensions`
    /// directly). Fires after constraint-source events and before type resolution.
    fn fire_extension_attached<O: TypeResolutionObserver>(
        &self,
        node: &SchemaNode,
        observer: &mut O,
    ) {
        for ext in &node.extensions {
            // Attribute the extension to its *injection-source* module when it was
            // annotation-injected (so a backend records the `*-ann` overlay, not
            // the extension's defining module); otherwise to its defining module.
            let source = ext.source_for_ns();
            if source != node.module_name {
                observer.on_extension_attached(source, ext, node);
            }
        }
    }

    /// Fire type-resolution events for a single node (§6 of the design doc).
    fn resolve_node_type<O: TypeResolutionObserver>(
        &self,
        node: &SchemaNode,
        cursor: &Cursor<'a>,
        observer: &mut O,
    ) {
        let type_stmt = match &node.kind {
            SchemaNodeKind::Leaf { type_stmt, .. }
            | SchemaNodeKind::LeafList { type_stmt, .. } => type_stmt,
            _ => return,
        };

        // Resolve the node's own type: fires `on_typedef_resolved` for the
        // typedef-derivation chain and each union member, in the order
        // `resolve_with_observer` already establishes.
        let resolved = self
            .types
            .resolve_with_observer(type_stmt, &node.module_name, observer);

        // For a leafref, additionally follow the path to its target so the
        // target's source module is reported via `on_leafref_resolved`. Errors
        // (e.g. a dangling leafref) are non-fatal: the walk continues.
        if matches!(resolved, Ok(ref rt) if rt.base == BuiltInType::Leafref) {
            let _ = self
                .types
                .follow_leafref_with_observer(type_stmt, cursor, observer);
        }
    }
}

#[cfg(test)]
mod tests;
