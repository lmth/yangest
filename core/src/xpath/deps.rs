// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! XPath dependency-path normalisation (feature #4).
//!
//! Transforms the location paths referenced by a `when`/`must` XPath expression
//! into normalised **dependency paths**: the cursor-relative keypaths whose
//! values the expression depends on, with an explicit per-step namespace.
//!
//! Two problems this solves (both named in the missing-features inventory):
//!
//!  * **`[.., <unknown>]` for sibling refs inside `case`.** An implementation
//!    that walks the *raw* schema tree counts `choice`/`case` as levels, so `..`
//!    from a node inside a case lands on the case and the sibling lookup fails.
//!    Normalisation drives a [`Cursor`], whose `..` is the *logical* parent
//!    (case/choice are transparent), so the dependency resolves correctly.
//!
//!  * **Lost namespace context across module boundaries.** A leafref/when path
//!    like `a:interface/b:ethernet/c:vlan-id` (each step from a different module)
//!    must keep each step's own namespace rather than collapsing to the current
//!    module's default. Each normalised step records the *matched node's* module,
//!    so the per-step namespaces are preserved.
//!
//! Opt-in: nothing here runs during compile (where XPath is only validated). A
//! backend calls [`compile_dep_paths`] when it needs dependency keypaths.

use indexmap::IndexMap;

use crate::cursor::{Axis, Cursor, QName as CursorQName};

use super::{Axis as XAxis, LocationPath, NodeTest, QName, Step, XPathExpr};

/// Namespace context for resolving the prefixes that appear in an XPath
/// expression: the prefix map of the module the expression is written in, plus
/// the module whose namespace applies to *unprefixed* names.
#[derive(Debug, Clone)]
pub struct NamespaceCtx {
    /// prefix → module name (the import prefixes of the defining module, plus
    /// the module's own prefix).
    pub prefixes: IndexMap<String, String>,
    /// The module whose namespace applies to unprefixed names. Per RFC 7950
    /// §6.4.1 this is the module in which the expression is *defined* (e.g. the
    /// grouping's module for a grouping-expanded node), not necessarily the
    /// module currently being emitted. See [`set_default_namespace`].
    pub default_module: String,
}

impl NamespaceCtx {
    /// Resolve a qname to the module that owns its namespace, or `None` if the
    /// prefix is unknown.
    pub fn resolve(&self, qname: &QName) -> Option<String> {
        match &qname.prefix {
            None => Some(self.default_module.clone()),
            Some(p) => self.prefixes.get(p).cloned(),
        }
    }
}

/// One step of a normalised dependency path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepStep {
    /// `.` — the context node.
    SelfStep,
    /// `..` — the logical parent (transparent `choice`/`case` already skipped).
    Parent,
    /// A named child, with the namespace (module) of the *matched* node.
    Child { ns: String, name: String },
}

/// A normalised dependency path: a sequence of cursor-relative steps, each
/// carrying its own namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepPath {
    /// True if the source location path was absolute (`/...`).
    pub absolute: bool,
    pub steps: Vec<DepStep>,
}

/// Apply the YANG default-namespace rule: an **unprefixed** name in an XPath
/// expression uses the namespace of the module in which the expression is
/// *defined* (`defining_module`), not the module currently being processed
/// (`current_module`). Returns the module to use, or `None` when the name is
/// prefixed (the caller resolves the prefix instead).
pub fn set_default_namespace(
    qname: &QName,
    defining_module: &str,
    _current_module: &str,
) -> Option<String> {
    match qname.prefix {
        None => Some(defining_module.to_string()),
        Some(_) => None,
    }
}

/// True if the dependency path is absolute.
pub fn is_path_absolute(dep: &DepPath) -> bool {
    dep.absolute
}

/// Convert a dependency path into cursor navigation axes.
pub fn dep_path_to_cursor_path(dep: &DepPath) -> Vec<Axis> {
    dep.steps
        .iter()
        .map(|s| match s {
            DepStep::SelfStep => Axis::SelfNode,
            DepStep::Parent => Axis::Parent,
            DepStep::Child { ns, name } => Axis::Child(CursorQName {
                module: Some(ns.clone()),
                name: name.clone(),
            }),
        })
        .collect()
}

/// Normalise every location path referenced by `expr` into a [`DepPath`],
/// resolving each path against `context` (the schema position the `when`/`must`
/// is attached to) so transparent `choice`/`case` nodes collapse and each step
/// keeps its own namespace.
///
/// Paths that cannot be resolved against the schema (dangling references,
/// unsupported axes) are skipped rather than aborting the whole expression.
pub fn compile_dep_paths(
    expr: &XPathExpr,
    ns_ctx: &NamespaceCtx,
    context: &Cursor<'_>,
) -> Vec<DepPath> {
    let mut location_paths = Vec::new();
    collect_location_paths(expr, &mut location_paths);

    let mut out = Vec::new();
    for lp in location_paths {
        if let Some(dp) = normalise_location_path(lp, ns_ctx, context) {
            out.push(dp);
        }
    }
    out
}

/// Recursively gather every `LocationPath` node in an XPath expression
/// (including those nested in predicates, function arguments, and operands).
fn collect_location_paths<'e>(expr: &'e XPathExpr, out: &mut Vec<&'e LocationPath>) {
    match expr {
        XPathExpr::Path(lp) => {
            out.push(lp);
            for step in &lp.steps {
                for pred in &step.predicates {
                    collect_location_paths(pred, out);
                }
            }
        }
        XPathExpr::Filter(base, preds) => {
            collect_location_paths(base, out);
            for p in preds {
                collect_location_paths(p, out);
            }
        }
        XPathExpr::FunctionCall { args, .. } => {
            for a in args {
                collect_location_paths(a, out);
            }
        }
        XPathExpr::Union(a, b)
        | XPathExpr::Or(a, b)
        | XPathExpr::And(a, b)
        | XPathExpr::Eq(a, b)
        | XPathExpr::Ne(a, b)
        | XPathExpr::Lt(a, b)
        | XPathExpr::Gt(a, b)
        | XPathExpr::Le(a, b)
        | XPathExpr::Ge(a, b)
        | XPathExpr::Add(a, b)
        | XPathExpr::Sub(a, b)
        | XPathExpr::Mul(a, b)
        | XPathExpr::Div(a, b)
        | XPathExpr::Mod(a, b) => {
            collect_location_paths(a, out);
            collect_location_paths(b, out);
        }
        XPathExpr::Neg(a) => collect_location_paths(a, out),
        XPathExpr::String(_) | XPathExpr::Number(_) | XPathExpr::Variable(_) => {}
    }
}

/// Normalise a single location path by driving a clone of `context`.
fn normalise_location_path(
    lp: &LocationPath,
    ns_ctx: &NamespaceCtx,
    context: &Cursor<'_>,
) -> Option<DepPath> {
    // For an absolute path, re-root at the module owning the first named step's
    // namespace (the data-tree root is the union of all modules).
    let mut cur = if lp.absolute {
        let first_mod = lp.steps.iter().find_map(|s| match &s.node_test {
            NodeTest::Name(q) => ns_ctx.resolve(q),
            _ => None,
        })?;
        context.reroot(&first_mod)?
    } else {
        context.clone()
    };

    let mut steps = Vec::with_capacity(lp.steps.len());
    for step in &lp.steps {
        normalise_step(step, ns_ctx, &mut cur, &mut steps)?;
    }
    Some(DepPath { absolute: lp.absolute, steps })
}

fn normalise_step(
    step: &Step,
    ns_ctx: &NamespaceCtx,
    cur: &mut Cursor<'_>,
    out: &mut Vec<DepStep>,
) -> Option<()> {
    match step.axis {
        XAxis::Self_ => {
            out.push(DepStep::SelfStep);
            Some(())
        }
        XAxis::Parent => {
            cur.move_to_parent().ok()?;
            out.push(DepStep::Parent);
            Some(())
        }
        XAxis::Child => {
            let NodeTest::Name(q) = &step.node_test else {
                // Wildcards / node()/text() etc. don't contribute a keypath dep.
                return None;
            };
            let module = ns_ctx.resolve(q)?;
            cur.move_to_child(&CursorQName {
                module: Some(module),
                name: q.local.clone(),
            })
            .ok()?;
            // The matched node's own module is the authoritative per-step namespace.
            let matched = cur.current()?;
            out.push(DepStep::Child {
                ns: matched.module_name.clone(),
                name: matched.name.clone(),
            });
            Some(())
        }
        // Other axes (ancestor, following-sibling, …) are not part of a YANG
        // keypath dependency and are dropped.
        _ => None,
    }
}

#[cfg(test)]
mod tests;
