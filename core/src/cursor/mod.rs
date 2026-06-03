// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! A stateful navigator over the *expanded, logical* schema tree (feature #1).
//!
//! The cursor is the shared mechanism for cross-node operations — leafref target
//! lookup, when/must dependency extraction, augment/deviation target lookup, and
//! validation of key references in keypaths. Without it, every backend
//! re-derives path traversal against `SchemaNode` trees by hand and the
//! case/choice-transparency rule (below) is re-implemented (and gets subtly
//! wrong) each time.
//!
//! ## Logical (case/choice-transparent) tree
//!
//! In a leafref `path` or an XPath expression, `choice` and `case` nodes are
//! **not** addressable: navigation proceeds as if they were spliced away (RFC
//! 7950 §6.5/§9.9.2). So a `when` on a `case` that references its sibling
//! `protocol` resolves to `[.]`, not `[.., <unknown>]`. This cursor exposes only
//! the *logical* tree: the children of a node are its data children with every
//! `choice` replaced by the (recursively flattened) children of its `case`s, and
//! the parent of a node is its nearest non-`choice`/`case` ancestor.
//!
//! ## Opt-in
//!
//! The cursor is constructed on demand by a backend over already-compiled
//! modules. It builds on the existing no-clone navigation primitives and
//! `SchemaNode::children`, and is never used by the compile path or by the tree
//! plugin — so non-consumers pay nothing.

use std::sync::Arc;

use crate::compiler::{CompiledModule, ExpansionCtx, PathStep, SchemaNode, SchemaNodeKind};

/// A resolved node identifier: a node name plus the *module that owns its
/// namespace*. `module == None` matches by local name only (used when the
/// namespace is already implied by context, e.g. unprefixed relative steps that
/// have not been namespace-qualified yet).
///
/// This is distinct from [`crate::xpath::QName`], which is prefix-based and
/// unresolved; the cursor navigates the resolved tree, so it matches on module
/// names (namespaces), not prefixes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QName {
    pub module: Option<String>,
    pub name: String,
}

impl QName {
    /// Match by local name only.
    pub fn bare(name: impl Into<String>) -> Self {
        QName { module: None, name: name.into() }
    }
    /// Match by `(module, name)`.
    pub fn qualified(module: impl Into<String>, name: impl Into<String>) -> Self {
        QName { module: Some(module.into()), name: name.into() }
    }
}

/// A single navigation step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Axis {
    /// `.` — stay put.
    SelfNode,
    /// `..` — move to the logical parent.
    Parent,
    /// A named child step.
    Child(QName),
}

/// Errors from cursor navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorError {
    /// No logical child with the requested name exists at the current position.
    NoSuchChild(String),
    /// `..` was attempted at the (pinned) root.
    AtRoot,
}

impl std::fmt::Display for CursorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CursorError::NoSuchChild(n) => write!(f, "no such child '{n}'"),
            CursorError::AtRoot => write!(f, "cannot move to parent: already at root"),
        }
    }
}

impl std::error::Error for CursorError {}

/// One level of the cursor's position stack: the logical-sibling list a node was
/// found in, plus the chosen index within it.
#[derive(Clone)]
struct Frame {
    siblings: Arc<Vec<SchemaNode>>,
    index: usize,
}

/// A stateful position in the logical schema tree of one module.
#[derive(Clone)]
pub struct Cursor<'a> {
    module: &'a CompiledModule,
    ctx: &'a ExpansionCtx<'a>,
    /// The module's logical top-level children (the root frame's siblings).
    root: Arc<Vec<SchemaNode>>,
    /// Position stack, root-first. Empty stack = positioned at the module root
    /// (no current node).
    stack: Vec<Frame>,
    /// Depth (stack length) below which `..` is refused. Set by
    /// [`set_xpath_root`](Self::set_xpath_root); 0 by default.
    pinned_root: usize,
}

impl<'a> Cursor<'a> {
    /// Create a cursor positioned at the root of `module`.
    pub fn root_of(module: &'a CompiledModule, ctx: &'a ExpansionCtx<'a>) -> Self {
        let root = Arc::new(flatten_transparent(module.children(ctx), ctx));
        Cursor { module, ctx, root, stack: Vec::new(), pinned_root: 0 }
    }

    /// The module this cursor navigates.
    pub fn module(&self) -> &'a CompiledModule {
        self.module
    }

    /// A fresh cursor rooted at the module named `module_name`, resolved through
    /// this cursor's expansion-context registry. Returns `None` if no such module
    /// exists. Used to resolve an *absolute* path whose first step lives in
    /// another module (the data-tree root is the union of all modules).
    pub fn reroot(&self, module_name: &str) -> Option<Cursor<'a>> {
        let module = self
            .ctx
            .registry
            .modules
            .values()
            .find(|m| m.key.name == module_name)?
            .as_ref();
        Some(Cursor::root_of(module, self.ctx))
    }

    /// The node at the current position, or `None` at the module root.
    pub fn current(&self) -> Option<&SchemaNode> {
        self.stack.last().map(|f| &f.siblings[f.index])
    }

    /// The logical parent of the current node, or `None` if the current node is
    /// a top-level node (its parent is the module root).
    pub fn parent(&self) -> Option<&SchemaNode> {
        if self.stack.len() < 2 {
            return None;
        }
        let f = &self.stack[self.stack.len() - 2];
        Some(&f.siblings[f.index])
    }

    /// Logical ancestors of the current node, root-first, excluding the current
    /// node itself.
    pub fn ancestors(&self) -> impl Iterator<Item = &SchemaNode> {
        let n = self.stack.len().saturating_sub(1);
        self.stack[..n].iter().map(|f| &f.siblings[f.index])
    }

    /// Current depth (number of steps from the module root).
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// True when positioned at the module root (no current node).
    pub fn at_root(&self) -> bool {
        self.stack.is_empty()
    }

    /// The logical children at the current position, including nodes spliced in
    /// by external augments targeting the current node (the data tree is the
    /// union of every module's contributions). `choice`/`case` are flattened.
    ///
    /// This is the same view `find_child` searches; a caller can enumerate it to
    /// drive its own traversal (e.g. the walker's own-augment pass).
    pub fn child_nodes(&self) -> Arc<Vec<SchemaNode>> {
        self.current_children()
    }

    /// The logical children available at the current position, including nodes
    /// spliced in by external augments targeting the current node (the data tree
    /// is the union of every module's contributions).
    fn current_children(&self) -> Arc<Vec<SchemaNode>> {
        match self.stack.last() {
            None => Arc::clone(&self.root),
            Some(f) => {
                let node = &f.siblings[f.index];
                let mut kids = flatten_transparent(node.children(self.ctx), self.ctx);
                let augmented = self.augment_children();
                if !augmented.is_empty() {
                    kids.extend(flatten_transparent(augmented, self.ctx));
                }
                Arc::new(kids)
            }
        }
    }

    /// The module-qualified absolute path of the current node, as
    /// `(module, name)` per step (root-first). Empty at the module root.
    fn current_abs_path(&self) -> Vec<(String, String)> {
        self.stack
            .iter()
            .map(|f| {
                let n = &f.siblings[f.index];
                (n.module_name.clone(), n.name.clone())
            })
            .collect()
    }

    /// Collect the nodes contributed by external augments (from any module in the
    /// registry) that target the current node. This is what lets the cursor reach
    /// cross-module augmented subtrees — essential for leafref/when paths in
    /// heavily-augmented module sets.
    ///
    /// Cost: a scan of every module's augment list per descent. The cursor is
    /// opt-in and used for targeted resolution, so this is acceptable; it never
    /// runs on the tree-plugin hot path.
    fn augment_children(&self) -> Vec<SchemaNode> {
        let abs = self.current_abs_path();
        if abs.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for module in self.ctx.registry.modules.values() {
            for aug in &module.augments {
                if augment_target_matches(module, &aug.target_path, &abs) {
                    // NOTE: `aug.nodes` may be a single `SchemaNodeKind::Uses`
                    // node (when the augment body is `uses g;`); see
                    // core-walker-design.md §11. Expanding it here is the
                    // natural fix but scales catastrophically on heavy
                    // bundles (the obvious approach was tried in `84da49f`
                    // and reverted in `<this commit>`); the recommended
                    // approach now is to cache expanded augment bodies in
                    // `ExpansionCtx` rather than re-expanding per descent.
                    out.extend(aug.nodes.iter().cloned());
                }
            }
        }
        out
    }

    /// Return a new cursor moved to the named logical child, or `None` if there
    /// is no such child. Non-mutating — useful for branching exploration.
    pub fn find_child(&self, qname: &QName) -> Option<Cursor<'a>> {
        let siblings = self.current_children();
        let index = match_index(&siblings, qname)?;
        let mut next = self.clone();
        next.stack.push(Frame { siblings, index });
        Some(next)
    }

    /// Move to the named logical child in place.
    pub fn move_to_child(&mut self, qname: &QName) -> Result<(), CursorError> {
        let siblings = self.current_children();
        let index = match_index(&siblings, qname)
            .ok_or_else(|| CursorError::NoSuchChild(qname.name.clone()))?;
        self.stack.push(Frame { siblings, index });
        Ok(())
    }

    /// Move to the logical parent in place.
    pub fn move_to_parent(&mut self) -> Result<(), CursorError> {
        if self.stack.len() <= self.pinned_root {
            return Err(CursorError::AtRoot);
        }
        self.stack.pop();
        Ok(())
    }

    /// Apply a single navigation step in place.
    pub fn move_step(&mut self, axis: &Axis) -> Result<(), CursorError> {
        match axis {
            Axis::SelfNode => Ok(()),
            Axis::Parent => self.move_to_parent(),
            Axis::Child(q) => self.move_to_child(q),
        }
    }

    /// Apply a sequence of navigation steps in place. On error the cursor is
    /// left at the last successfully-reached position.
    pub fn follow_path(&mut self, steps: &[Axis]) -> Result<(), CursorError> {
        for step in steps {
            self.move_step(step)?;
        }
        Ok(())
    }

    /// Return to the module root (clears the position stack and any pin).
    pub fn reset_to_root(&mut self) {
        self.stack.clear();
        self.pinned_root = 0;
    }

    /// Pin the current depth as the root for relative resolution: subsequent
    /// `..` steps will refuse to move above this position. Used when resolving a
    /// relative XPath/leafref from a fixed context node.
    pub fn set_xpath_root(&mut self) {
        self.pinned_root = self.stack.len();
    }

    /// Return to the pinned xpath root (see [`set_xpath_root`](Self::set_xpath_root)).
    pub fn reset_to_xpath_root(&mut self) {
        self.stack.truncate(self.pinned_root);
    }

    /// Convert into a floating cursor seeded with this single position.
    pub fn floating(self) -> FloatingCursor<'a> {
        FloatingCursor { positions: vec![self] }
    }
}

/// A cursor tracking a *set* of candidate positions rather than one. Used while
/// validating a relative XPath whose context node is not unique (e.g. a grouping
/// instantiated at several points).
#[derive(Clone)]
pub struct FloatingCursor<'a> {
    positions: Vec<Cursor<'a>>,
}

impl<'a> FloatingCursor<'a> {
    /// Seed a floating cursor from explicit candidate positions.
    pub fn from_positions(positions: Vec<Cursor<'a>>) -> Self {
        FloatingCursor { positions }
    }

    /// The current candidate positions.
    pub fn positions(&self) -> &[Cursor<'a>] {
        &self.positions
    }

    /// True when no candidate positions remain.
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    /// Apply a step to every candidate, dropping candidates for which it fails.
    pub fn move_step(&mut self, axis: &Axis) {
        self.positions = std::mem::take(&mut self.positions)
            .into_iter()
            .filter_map(|mut c| c.move_step(axis).ok().map(|_| c))
            .collect();
    }

    /// Apply a sequence of steps to every candidate.
    pub fn follow_path(&mut self, steps: &[Axis]) {
        for step in steps {
            self.move_step(step);
        }
    }
}

/// Flatten `choice`/`case` nodes so the result is the logical data-child list:
/// every `choice` is replaced by the (recursively flattened) children of its
/// `case`s, so its grandchildren appear as direct children.
fn flatten_transparent(nodes: Vec<SchemaNode>, ctx: &ExpansionCtx<'_>) -> Vec<SchemaNode> {
    // Avoid the per-node `children` call entirely when there are no transparent
    // nodes (the common case) — just return the list unchanged.
    if !nodes
        .iter()
        .any(|n| matches!(n.kind, SchemaNodeKind::Choice { .. } | SchemaNodeKind::Case { .. }))
    {
        return nodes;
    }
    let mut out = Vec::with_capacity(nodes.len());
    for n in nodes {
        match &n.kind {
            SchemaNodeKind::Choice { .. } | SchemaNodeKind::Case { .. } => {
                let inner = n.children(ctx);
                out.extend(flatten_transparent(inner, ctx));
            }
            _ => out.push(n),
        }
    }
    out
}

/// True if `target_path` (an augment target in `aug_module`, with prefixed
/// steps) resolves to the module-qualified absolute path `abs`.
fn augment_target_matches(
    aug_module: &CompiledModule,
    target_path: &[PathStep],
    abs: &[(String, String)],
) -> bool {
    if target_path.len() != abs.len() {
        return false;
    }
    target_path.iter().zip(abs).all(|(step, (mod_name, name))| {
        &step.name == name && step_module(aug_module, step).as_deref() == Some(mod_name.as_str())
    })
}

/// Resolve the module name a target-path step refers to, in the context of the
/// augmenting module (unprefixed / own-prefix → that module; else its imports).
fn step_module(aug_module: &CompiledModule, step: &PathStep) -> Option<String> {
    match &step.prefix {
        None => Some(aug_module.key.name.clone()),
        Some(p) if *p == aug_module.prefix => Some(aug_module.key.name.clone()),
        Some(p) => aug_module.prefix_map.get(p).cloned(),
    }
}

/// Find the first node in `siblings` matching `qname` (by name, and by namespace
/// module when specified).
fn match_index(siblings: &[SchemaNode], qname: &QName) -> Option<usize> {
    siblings.iter().position(|n| {
        n.name == qname.name
            && match &qname.module {
                None => true,
                Some(m) => &n.module_name == m,
            }
    })
}

#[cfg(test)]
mod tests;
