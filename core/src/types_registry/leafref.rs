// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Leafref target resolution (feature #2).
//!
//! Given a leafref's `path` and a starting [`Cursor`] positioned at the leaf
//! that carries the leafref type, walk to the target schema node and resolve the
//! target's type via the [`TypeRegistry`]. This bottoms a leafref out at the
//! resolved target type (e.g. a string with a length restriction), follows
//! leafref→leafref chains to their ultimate base, and reports whether the target
//! is a `leaf-list` (so a backend can emit `{list, target_type}` rather than
//! `{list, undefined}`).
//!
//! Built on the cursor (#1, which makes `choice`/`case` transparent so sibling
//! references resolve correctly) and the type registry (#3, which flattens the
//! target's typedef chain).

use crate::ast::{BuiltInKeyword, Stmt};
use crate::compiler::{SchemaNode, SchemaNodeKind};
use crate::cursor::{Axis, Cursor, QName as CursorQName};
use crate::xpath::{parse_xpath, Axis as XAxis, NodeTest, XPathExpr};

use super::{BuiltInType, NoopObserver, ResolvedType, TypeError, TypeRegistry, TypeResolutionObserver};

/// The result of resolving a leafref.
#[derive(Debug, Clone)]
pub struct LeafrefTarget {
    /// The immediate target schema node the `path` points at.
    pub target_node: SchemaNode,
    /// The target's fully-resolved type, with any leafref→leafref chain followed
    /// to its ultimate (non-leafref) base.
    pub target_type: ResolvedType,
    /// True when the target is a `leaf-list` (vs a `leaf`).
    pub is_leaf_list: bool,
    /// The `require-instance` value of the *source* leafref (default true).
    pub require_instance: bool,
}

/// Maximum leafref→leafref chain length before we declare a cycle.
const MAX_LEAFREF_DEPTH: usize = 32;

impl<'a> TypeRegistry<'a> {
    /// Resolve a leafref's target node and type. `from` must be positioned at the
    /// leaf that carries `leafref_type`.
    pub fn follow_leafref(
        &self,
        leafref_type: &Stmt,
        from: &Cursor<'a>,
    ) -> Result<LeafrefTarget, TypeError> {
        let mut obs = NoopObserver;
        self.follow_leafref_with_observer(leafref_type, from, &mut obs)
    }

    /// Like [`follow_leafref`](Self::follow_leafref) but reports the resolved
    /// target to `observer` (feature #7).
    pub fn follow_leafref_with_observer(
        &self,
        leafref_type: &Stmt,
        from: &Cursor<'a>,
        observer: &mut dyn TypeResolutionObserver,
    ) -> Result<LeafrefTarget, TypeError> {
        self.follow_leafref_inner(leafref_type, from, observer, 0)
    }

    fn follow_leafref_inner(
        &self,
        leafref_type: &Stmt,
        from: &Cursor<'a>,
        observer: &mut dyn TypeResolutionObserver,
        depth: usize,
    ) -> Result<LeafrefTarget, TypeError> {
        if depth > MAX_LEAFREF_DEPTH {
            return Err(TypeError::UnsupportedLeafrefPath(
                "leafref chain too deep (cycle?)".into(),
            ));
        }
        // The leafref `type` argument must be `leafref`, and it must carry a `path`.
        if leafref_type.arg.as_deref().map(strip_prefix) != Some("leafref") {
            return Err(TypeError::NotALeafref);
        }
        let path = leafref_type
            .get_substmt(BuiltInKeyword::Path)
            .and_then(|p| p.arg.as_deref())
            .ok_or_else(|| TypeError::UnsupportedLeafrefPath("leafref has no path".into()))?;

        // Prefixes in the path are resolved in the namespace of the module that
        // defines the leaf carrying the leafref — i.e. the cursor's current node.
        let source_module = from
            .current()
            .map(|n| n.module_name.clone())
            .ok_or_else(|| {
                TypeError::LeafrefTargetNotFound("leafref not positioned at a node".into())
            })?;

        let (absolute, axes) = self.path_to_axes(path, &source_module)?;

        // For an absolute path the data-tree root is the union of all modules, so
        // re-root at the module owning the first step's namespace; a single-module
        // cursor can't otherwise reach another module's top-level nodes. For a
        // relative path the context is the leaf itself (`..` walks up from it).
        let mut cur = if absolute {
            let first_mod = axes.iter().find_map(|a| match a {
                Axis::Child(q) => q.module.clone(),
                _ => None,
            });
            match first_mod {
                Some(m) => from.reroot(&m).ok_or_else(|| {
                    TypeError::LeafrefTargetNotFound(format!("{path}: module '{m}' not found"))
                })?,
                None => {
                    let mut c = from.clone();
                    c.reset_to_root();
                    c
                }
            }
        } else {
            from.clone()
        };
        cur.follow_path(&axes)
            .map_err(|e| TypeError::LeafrefTargetNotFound(format!("{path}: {e}")))?;

        let target = cur
            .current()
            .ok_or_else(|| TypeError::LeafrefTargetNotFound(path.to_string()))?
            .clone();

        let (target_type_stmt, is_leaf_list) = match &target.kind {
            SchemaNodeKind::Leaf { type_stmt, .. } => (type_stmt.clone(), false),
            SchemaNodeKind::LeafList { type_stmt, .. } => (type_stmt.clone(), true),
            _ => {
                return Err(TypeError::LeafrefTargetNotLeaf {
                    name: target.name.clone(),
                });
            }
        };

        observer.on_leafref_resolved(&source_module, &target);

        let mut target_type = self.resolve(&target_type_stmt, &target.module_name)?;

        // Follow a leafref→leafref chain to its ultimate base type.
        if target_type.base == BuiltInType::Leafref {
            let inner = self.follow_leafref_inner(&target_type_stmt, &cur, observer, depth + 1)?;
            target_type = inner.target_type;
        }

        Ok(LeafrefTarget {
            target_node: target,
            target_type,
            is_leaf_list,
            require_instance: super::require_instance(leafref_type),
        })
    }

    /// Parse a leafref `path` into `(absolute, cursor axes)`, resolving each
    /// step's prefix against `source_module`. Predicates are ignored: they
    /// constrain key values but never change which node the path designates.
    fn path_to_axes(
        &self,
        path: &str,
        source_module: &str,
    ) -> Result<(bool, Vec<Axis>), TypeError> {
        let expr = parse_xpath(path)
            .map_err(|e| TypeError::UnsupportedLeafrefPath(format!("{path}: {e}")))?;
        let lp = match expr {
            XPathExpr::Path(lp) => lp,
            // `deref(...)`, `current()/...`, unions, etc. are not yet supported.
            _ => return Err(TypeError::UnsupportedLeafrefPath(path.to_string())),
        };

        let mut axes = Vec::with_capacity(lp.steps.len());
        for step in &lp.steps {
            match step.axis {
                XAxis::Self_ => axes.push(Axis::SelfNode),
                XAxis::Parent => axes.push(Axis::Parent),
                XAxis::Child => {
                    let NodeTest::Name(q) = &step.node_test else {
                        return Err(TypeError::UnsupportedLeafrefPath(format!(
                            "{path}: non-name node test"
                        )));
                    };
                    let module = match &q.prefix {
                        Some(p) => self.module_for_prefix(source_module, p)?,
                        // Unprefixed name → the namespace of the defining module.
                        None => source_module.to_string(),
                    };
                    axes.push(Axis::Child(CursorQName {
                        module: Some(module),
                        name: q.local.clone(),
                    }));
                }
                other => {
                    return Err(TypeError::UnsupportedLeafrefPath(format!(
                        "{path}: unsupported axis {other:?}"
                    )));
                }
            }
        }
        Ok((lp.absolute, axes))
    }
}

/// Strip a `prefix:` from a type argument, leaving the local name (built-in type
/// names like `leafref` are never prefixed, but a defensive strip is harmless).
fn strip_prefix(arg: &str) -> &str {
    arg.rsplit(':').next().unwrap_or(arg)
}
