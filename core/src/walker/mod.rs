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
//! firing). Cross-module **augment splicing and deviation inlining** at the
//! walk level (step 3) depend on indexset-based augment splicing in `compile`
//! and are not yet performed here: the walk currently visits each module's own
//! `children(ctx)`. [`WalkOptions::follow_deviations`] and
//! [`WalkOptions::deep_typedef_chains`] are accepted and stored but reserved for
//! that later work.
//!
//! Like the rest of the resolution machinery, the walker is **opt-in**: it is
//! constructed by a backend from what `Plugin::emit_module` already has, and
//! nothing in the compile path uses it.

use crate::compiler::{CompiledModule, ExpansionCtx, ModuleRegistry, SchemaNode, SchemaNodeKind};
use crate::cursor::{Cursor, QName};
use crate::types_registry::{BuiltInType, TypeRegistry, TypeResolutionObserver};

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

    /// Walk the module's schema tree, firing observer events in encounter order.
    /// The observer is borrowed for the duration of the walk and may carry
    /// arbitrary backend state.
    pub fn walk<O: TypeResolutionObserver>(&self, observer: &mut O) {
        self.walk_with_visitor(observer, |_| {});
    }

    /// Walk while exposing the cursor at every visit. The `visit` closure is
    /// called *before* type resolution at each node; type resolution still fires
    /// through `observer`.
    pub fn walk_with_visitor<O, V>(&self, observer: &mut O, mut visit: V)
    where
        O: TypeResolutionObserver,
        V: FnMut(&Cursor<'_>),
    {
        let root = Cursor::root_of(self.module, self.ctx);
        for child in self.module.children(self.ctx) {
            self.visit_node(&child, &root, observer, &mut visit);
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
        self.resolve_node_type(node, &node_cursor, observer);

        for child in node.children(self.ctx) {
            self.visit_node(&child, &node_cursor, observer, visit);
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
