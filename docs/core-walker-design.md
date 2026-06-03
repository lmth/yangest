# Core Schema Walker Design

A backend that needs to *reproduce* the reference compiler's
`ns_to_prefix_maps` order — or any other byte-identical artefact whose
content depends on the order in which the compiler resolved types and
leafrefs — cannot use a fully-built compiled tree as its starting point.
The encounter order is information that is *thrown away* once the
`CompiledModule` is finalised: it must be observed *during* traversal.

The existing `TypeResolutionObserver` (see
[`core::types_registry::observer`](../core/src/types_registry/observer.rs))
already exposes the *event* surface: every typedef and leafref
resolution fires a callback. What it does not yet expose is a *driver*
— a canonical visitor that walks the compiled schema in the order the
reference compiler walks it, calling `TypeRegistry::resolve_with_observer`
at the right moments.

This document specifies that driver: the **`SchemaWalker`** — and its
contract with backends. It is the missing piece between the
already-present observer and a working byte-faithful (or any
order-sensitive) backend.

---

## Table of Contents

1. [Motivation](#1-motivation)
2. [What "encounter order" actually means](#2-what-encounter-order-actually-means)
3. [Goals and non-goals](#3-goals-and-non-goals)
4. [Public surface](#4-public-surface)
5. [Visit order specification](#5-visit-order-specification)
6. [Resolution points](#6-resolution-points)
7. [Default vs custom walkers](#7-default-vs-custom-walkers)
8. [Backend integration pattern](#8-backend-integration-pattern)
9. [Testing strategy](#9-testing-strategy)
10. [Migration plan](#10-migration-plan)

---

## 1. Motivation

A plugin that wants its output to match the reference compiler byte-for-byte
must reproduce decisions the reference compiler made *during* compilation,
not just observe their finalised state. The clearest example is
`ns_to_prefix_maps`: the reference compiler builds it by appending each
new (namespace, prefix) pair the *first time* a typedef from that
namespace is resolved or a leafref crosses into that namespace. The
final list is therefore in *resolution-encounter order*, which depends
on the order schema nodes are visited and the order types within each
node are resolved.

Three independent attempts to derive that order from a fully-compiled
tree (sorted alphabetical, sorted reverse-alphabetical, post-pass DFS
encounter order) all failed on real yangbundles: the order is genuinely
the interleaving of the reference compiler's visit pattern, augment
splicing, and typedef-chain walking. The only way to reproduce it is
to walk the schema in the same order as the reference compiler and to
fire `TypeResolutionObserver` callbacks as we go.

`SchemaWalker` is the canonical visitor that does this. Backends that
need encounter-order data simply provide an observer; everything else
they receive is identical to what the existing `Plugin::emit_module`
contract already gives them.

---

## 2. What "encounter order" actually means

The reference compiler's order is not a single tree-walk pattern.
Captured behaviour from the reference yangbundle outputs shows
the following composite ordering rules:

1. **Per-module entry point**: the walker starts at the module root
   and visits the module's *own* schema-tree children in
   declaration order (the order the statements appear in the source
   `.yang` file, after grouping expansion and deviation application —
   i.e., the order preserved in `CompiledModule::children`).
2. **Augment splicing**: when an augment is applied to a node that
   belongs to another module, the augmented children appear *inline*
   at the augment-target position, in the order the augment statements
   were applied. The walker descends into the augment's children
   *while inside the augmented node*, then resumes the host module's
   children. The augment's source module is exposed to the observer
   when its first child is visited, not when the augment statement
   was originally compiled.
3. **Type resolution at each schema node** happens in this order, *for
   every leaf, leaf-list, typedef-bearing node visited*:
     a. base type chain (innermost first → outermost),
     b. union member types (in declaration order),
     c. for `leafref`: the path target is *resolved* (cursor follows
        the leafref path); the target's own type is then resolved
        recursively, but *only one level deep* — chains of leafrefs
        through leafrefs do not multi-fire.
4. **Recurse into children**: after the node's own type is fully
   resolved, descend into children (containers, lists, choice/case,
   …) in declaration order. This is critical: the reference compiler
   resolves a node's type *before* descending, not after.
5. **Annotation and deviation modules**: when a node carries a
   `when`/`must` whose source-module is a deviation or annotation, the
   walker exposes that module to the observer at the visit of the
   carrying node, *not* at root level. This is what causes annotation
   modules to appear in the middle of `ns_to_prefix_maps` lists rather
   than at the start.
6. **No revisit**: each `(module, schema-path)` pair is visited at most
   once. Augments do not cause double-visits of their target node;
   they extend the children list.

This ordering is what the SchemaWalker contract guarantees.

---

## 3. Goals and non-goals

### Goals

* Provide a single canonical schema-walker that drives
  `resolve_with_observer` in the order the reference compiler does.
* Keep the `TypeResolutionObserver` trait unchanged — backends that
  already implement it (none in-tree today) gain a drop-in driver.
* Be cheap when no observer is supplied: the walker takes a generic
  `&mut O: TypeResolutionObserver`, and the no-op observer's empty
  methods are inlined away (same property the existing
  `resolve_with_observer` already enjoys).
* Expose the cursor and the registry at every visit, so backends can
  reuse the walker for any schema-traversal job — not just
  ns-map building. The walker becomes the natural place for any
  visitor that today does its own ad-hoc DFS.
* Allow a backend to *customise* the visit pattern when it genuinely
  needs to deviate (e.g. a tree-printer that wants choice/case
  flattened differently). This is opt-in via a hook, not a fork.

### Non-goals

* Changing or extending the `TypeResolutionObserver` event set. Adding
  events (e.g. `on_node_enter`) is a separate, later proposal; this
  document is strictly about the driver.
* Replacing `Plugin::emit` or `Plugin::emit_module`. The walker is a
  utility plugins call from inside `emit_module`, not a new top-level
  hook.
* Reproducing every micro-detail of the reference compiler. The
  contract is "encounter order matches for typedef/leafref events";
  ordering of nodes that produce *no* type events is unconstrained.

---

## 4. Public surface

A new module `core::walker` is added with the following surface.

```rust
// core/src/walker/mod.rs

use crate::compiler::{CompiledModule, ExpansionCtx, ModuleRegistry, SchemaNode};
use crate::cursor::Cursor;
use crate::types_registry::{TypeRegistry, TypeResolutionObserver};

/// Drives a schema walk that fires `TypeResolutionObserver` events in the
/// reference compiler's encounter order. See `docs/core-walker-design.md`
/// for the visit-order contract.
pub struct SchemaWalker<'a> {
    module: &'a CompiledModule,
    registry: &'a ModuleRegistry,
    types: &'a TypeRegistry,
    ctx: &'a ExpansionCtx<'a>,
    options: WalkOptions,
}

#[derive(Default, Clone, Copy)]
pub struct WalkOptions {
    /// Visit deviation nodes inline at their target position. Default true,
    /// matches reference compiler. Set false to skip deviated subtrees.
    pub follow_deviations: bool,
    /// Fire observer events for typedef chains nested inside an
    /// already-resolved typedef. Default false (matches reference compiler:
    /// the resolved-typedef cache short-circuits).
    pub deep_typedef_chains: bool,
}

impl<'a> SchemaWalker<'a> {
    pub fn new(
        module: &'a CompiledModule,
        registry: &'a ModuleRegistry,
        types: &'a TypeRegistry,
        ctx: &'a ExpansionCtx<'a>,
    ) -> Self { /* … */ }

    pub fn with_options(mut self, options: WalkOptions) -> Self { /* … */ }

    /// Walk the module's schema tree, firing observer events in
    /// encounter order. The observer is borrowed for the duration of
    /// the walk; it may carry arbitrary backend state.
    pub fn walk<O: TypeResolutionObserver>(&self, observer: &mut O);

    /// Walk while exposing the cursor at every visit. The visitor
    /// closure is called *before* type resolution at the node — type
    /// resolution still fires through the observer.
    pub fn walk_with_visitor<O, V>(&self, observer: &mut O, visit: V)
    where
        O: TypeResolutionObserver,
        V: FnMut(&Cursor<'_>);
}
```

`WalkOptions::default()` returns the reference-compiler-faithful
configuration. Backends customise only when they explicitly need to.

The walker is constructible from anything a `Plugin::emit_module`
already has access to — no new state has to be threaded through the
pipeline.

---

## 5. Visit order specification

For a single-module walk:

```
walk(module):
    cursor := Cursor::root_of(module, ctx)
    for child in module.schema_children:        # declaration order
        visit(cursor.descend(child))

visit(cursor):
    fire_visitor(cursor)
    resolve_node_type(cursor)                   # see §6
    for child in cursor.current().schema_children:
        visit(cursor.descend(child))
```

`schema_children` is the *post-augment, post-grouping, post-deviation*
ordered child list — i.e., the canonical order already stored on
`SchemaNode`. The walker performs no further reordering. Any
order-sensitivity in the reference compiler's output is therefore
resolved by getting `schema_children` right at compile time, not at
walk time. (A separate proposal — feature #8, indexset-based augment
splicing — covers this.) The walker's contract is "produce events in
the order I see children", which is sufficient because compilation
already arranges children correctly.

For multi-module emission (e.g. a yangbundle), backends call `walk`
once per display module in the order the user supplied them; the
walker is stateless across calls.

---

## 6. Resolution points

At each visited node, type resolution fires in this order:

```
resolve_node_type(cursor):
    node := cursor.current()
    if node has a type (leaf, leaf-list, typedef, …):
        types.resolve_with_observer(node.type, node.module, observer)
    if node has a when expression:
        for dep in node.when.explicit_deps:
            observer.note_when_module(dep.source_module)
            # informational; backends may ignore
    if node has must expression(s):
        same as when
```

`resolve_with_observer` is the existing entry point on `TypeRegistry`;
its ordering of typedef-chain walks and union-member iteration is
already correct (it is what produced the only working baseline today).
The walker's responsibility is solely to call it at the right *outer*
visit order.

The `note_when_module` hook is *not* added to the trait in this design
— it remains out of scope. Backends that need it observe `when`
modules through the existing `AppliedDeviations`/`AppliedAnnotations`
pdata. The walker just passes them through; expanding the trait is
deferred until a second backend genuinely needs it.

---

## 7. Default vs custom walkers

The default walker captures the reference compiler's behaviour exactly.
Backends that need a different visit pattern (e.g. the `tree` plugin's
flattened choice/case rendering) should **not** modify `SchemaWalker`;
they should walk the schema themselves with their own DFS, exactly as
they do today, and call `types.resolve_with_observer` directly at each
node where they need observer events fired.

Customisation surface on `SchemaWalker` is intentionally narrow
(`WalkOptions`). If a backend needs more, that is a strong signal it
should be a hand-rolled walker — the canonical walker is for backends
trying to *match* the reference compiler.

---

## 8. Backend integration pattern

A backend that wants encounter-order data implements
`TypeResolutionObserver` on a struct that owns its accumulator, then
drives the walker from inside `emit_module`:

```rust
struct NsMapCollector {
    seen: indexmap::IndexSet<(String, String)>,   // (module, prefix)
    own_module: String,
}

impl TypeResolutionObserver for NsMapCollector {
    fn on_typedef_resolved(&mut self, src: &str, _td: &Typedef) {
        if src != self.own_module {
            self.seen.insert((src.to_owned(), prefix_for(src)));
        }
    }
    fn on_leafref_resolved(&mut self, src: &str, target: &SchemaNode) {
        let m = target.module().name();
        if m != self.own_module {
            self.seen.insert((m.to_owned(), prefix_for(m)));
        }
    }
}

// inside Plugin::emit_module:
let mut collector = NsMapCollector::new(module.name());
SchemaWalker::new(module, registry, types, ctx).walk(&mut collector);
write_ns_to_prefix_maps(&collector.seen, &mut output);
```

A byte-faithful backend's current `build_ns_to_prefix_maps` —
`collect_source_modules`, the manual deviation/annotation insertions,
the `intermediate.reverse()` post-pass — all collapse into the
collector's `seen` field. That is the goal.

---

## 9. Testing strategy

Three layers of tests live alongside the walker:

1. **Unit tests** in `core/src/walker/tests.rs` exercise visit order
   on small hand-built `CompiledModule`s: declaration order, augment
   splicing, deviation inlining, union member order, leafref-target
   chasing.
2. **Observer-event tests** instantiate a recording observer, walk a
   fixture module, and assert the exact event sequence. These tests
   pin the encounter-order contract — any change to the walker that
   alters event order without changing these tests is a contract
   violation.
3. **Reference-parity tests** (in `tests/`, gated on the `ref-data`
   feature so CI without reference outputs still passes) walk a fixed
   set of modules from a checked-in yangbundle and assert that the
   collected `ns_to_prefix_maps` matches the reference output exactly.
   These are end-to-end and slow; they are the canary for regressions
   in real backends.

A byte-faithful backend's existing bundle-test harness becomes the
de-facto acceptance test for this work: passing all 461 modules requires
the walker contract to hold for the full reference yangbundle.

---

## 10. Migration plan

**Status:** steps 1–2 are implemented in [`core::walker`](../core/src/walker/mod.rs)
(`SchemaWalker`, `WalkOptions`, `walk`, `walk_with_visitor`, observer firing at
type-resolution sites) with the §9.1/§9.2 unit and observer-event tests. Steps 3
(augment-splicing / deviation-inlining at the walk level) and 4 (`ref-data`-gated
reference-parity tests) are not yet done: step 3 depends on indexset-based augment
splicing in `compile`, and step 4 needs checked-in reference outputs.

This is upstream-only design work. Implementation proceeds in four
commits, each independently mergeable:

1. **Add `core::walker` module skeleton** — `SchemaWalker`,
   `WalkOptions`, `walk` and `walk_with_visitor` methods. The walker
   compiles and runs but visits in declaration order *without* firing
   observer events at type-resolution sites. Existing tests pass; no
   backend uses it yet.
2. **Wire observer firing at resolution points** — `walk` calls
   `types.resolve_with_observer` at each node with a type. Add the
   unit and observer-event tests from §9.1 and §9.2. No backend
   migrated yet.
3. **Augment-splicing and deviation-inlining ordering** — make
   `WalkOptions::follow_deviations` actually inline deviation subtrees
   and verify augment children appear at their splice site. This is
   the work that depends on feature #8 (indexset-based augment
   splicing in `compile`); if #8 is already in flight, this commit
   blocks on it.
4. **Reference-parity tests** — add the `ref-data`-gated tests in §9.3
   using a small representative slice of the reference yangbundle, so
   regressions show up in the upstream CI rather than waiting for the
   downstream backend migration to flag them.

After step 4 the upstream is ready for byte-faithful backends. The
downstream migration (rewriting the backend's `build_ns_to_prefix_maps`
against `SchemaWalker`) happens separately and does not touch upstream
further.

---

## 11. Step 3 detail — what augment splicing must actually do

With steps 1–2 in place, the walker visits `module.children(ctx)`. By
construction those children include subtrees that *other* modules
augmented into this module — they appear inline at their splice point,
correctly carrying the augmenting module's `module_name` on each
`SchemaNode`. So an observer driving the walker on module *M* already
sees every typedef and leafref of an augment splice-in into *M* and
correctly reports the augmenting module as the type's source module.

What the walker does **not** do is, when walking module *M*, descend
into augments *M itself applies into other modules*. The reference
compiler's per-module `ns_to_prefix_maps` for module *M* covers the
types of leafs *M* introduces *anywhere* — including under augments
*M* contributes to other modules. So step 3 is more naturally framed
as **own-augment traversal**, not "splicing":

```rust
impl<'a> SchemaWalker<'a> {
    /// Visit the module's *own* schema tree (current `walk` behaviour).
    pub fn walk_own_tree<O: TypeResolutionObserver>(&self, observer: &mut O);

    /// For every augment `module` applies into another module's tree, walk
    /// the augment's children and fire observer events as if the children
    /// were part of `module`. The cursor is positioned at the augment
    /// target so leafref paths resolve correctly.
    pub fn walk_own_augments<O: TypeResolutionObserver>(&self, observer: &mut O);

    /// Convenience: `walk_own_tree` followed by `walk_own_augments`.
    /// This becomes the new behaviour of `walk`.
    pub fn walk<O: TypeResolutionObserver>(&self, observer: &mut O);
}
```

Splitting the methods is mandatory, not cosmetic: a backend that is
already finalising per-module output (the fxs case) must complete
**all** observer events for module *M* before the next module is
walked, which means own-augment events have to fire during *M*'s walk
even though structurally they live under another module's tree.

### Where the augment data lives

The per-module list of own augments is already available via
`CompiledModule::augments` (alternatively the existing
`AugmentEntry` / overlay structures — choose whichever exposes the
augment target path *and* the augment children in source-declaration
order). Walking own-augments is therefore a structural traversal over
already-compiled data; it does not need any new compile-time work.
The walker positions the cursor at the augment target via
`Cursor::root_of(<host_module>, ctx).follow_path(<augment_path>)`,
then iterates `augment.children` in declaration order, calling
`visit_node` exactly as for the own-tree walk.

### Observer-side semantics (no trait change required)

Critically, no changes to `TypeResolutionObserver` are needed for
step 3. The trait fires events on `(source_module, …)`; the walker
already records the *correct* source module on each `SchemaNode` it
visits. Whether a node was reached through `walk_own_tree` or
`walk_own_augments` is invisible to the observer — exactly what an
order-encounter consumer wants.

---

## 12. Step 5 — when/must source-module exposure (new step)

Section 6 of the spec deferred exposing `when`/`must` source modules
because the trait-extension cost was uncertain. Empirical evidence
from a 461-module reference yangbundle now refutes that deferral: of
21 modules whose `ns_to_prefix_maps` differs from the reference,
**three have wrong membership** — meaning typedef+leafref events
alone cannot reproduce the reference set. The missing signal is the
source module of `when`/`must` constraints contributed by deviations
and annotations applied to nodes during compilation.

Concretely, for `Cisco-IOS-XE-native` the reference includes a
deviation that adds a `when` whose source module is the deviation
module itself — the reference compiler counts this in the namespace
map even though the deviation contributes no typed leaf of its own.
This must be observable via the walker.

Proposed extension (default-method, backward compatible):

```rust
pub trait TypeResolutionObserver {
    fn on_typedef_resolved(&mut self, _source_module: &str, _typedef: &Typedef) {}
    fn on_leafref_resolved(&mut self, _source_module: &str, _target: &SchemaNode) {}

    /// Fired for the source module of every `when`/`must` constraint visited
    /// at a node, whether the constraint was original or contributed by a
    /// deviation/annotation. Default impl is empty so existing observers do
    /// not change behaviour.
    fn on_constraint_source(
        &mut self,
        _source_module: &str,
        _kind: ConstraintKind,
        _node: &SchemaNode,
    ) {}
}

#[derive(Clone, Copy, Debug)]
pub enum ConstraintKind { When, Must }
```

The walker fires `on_constraint_source` once per `(source_module,
kind)` pair encountered at a node, *before* descending into the node's
children. Backends that do not care provide the default no-op and pay
nothing.

This event is exactly what the fxs plugin's existing
`AppliedDeviations`/`AppliedAnnotations` pdata reads today — but now
tied to encounter order, which is the missing dimension.

The migration plan grows a fifth step:

5. **Add `on_constraint_source` event** — extend the trait, fire from
   the walker at each visited node carrying `when`/`must`. Add
   observer-event tests covering: (a) original constraint, (b)
   deviation-injected constraint, (c) annotation-injected constraint,
   (d) multi-source nodes (a node with constraints from two different
   modules).

Step 5 is independent of step 3 and can land before or after it.

---

## 13. Open questions (revised)

* Whether to expose `WalkOptions::deep_typedef_chains`. The reference
  compiler does not deep-walk; no current backend needs it. Add it
  only if a second backend asks. **Status: unchanged.**
* Whether `walk_with_visitor`'s `V: FnMut` should also be able to
  *prune* a subtree (`enum Action { Recurse, Skip }`). The first
  consumer (fxs ns-map building) does not need pruning; the `tree`
  plugin would. Add when a second consumer appears.
  **Status: still deferred; no urgency.**
* Augment-source-module exposure via `on_node_enter`. **Resolved**
  by §11: own-augment traversal makes the source-module visible
  through existing `on_typedef_resolved` / `on_leafref_resolved`
  events; no `on_node_enter` event is needed.
* Whether `on_constraint_source` should fire once per visited node
  *or* once per (source-module, node, kind) triple. Once per triple
  is safer (it lets the observer dedup later); but it loses the
  signal "two musts from the same module". Recommend **once per
  constraint statement** — the natural semantic.
* Cursor positioning during `walk_own_augments`: the augment target
  may be inside a `case` of a `choice` in the host module. The walker
  must use the existing transparent-choice/case logic so leafref
  paths under the augment resolve through the host's data tree.

---

## 14. Verification plan for the partial implementation

Before step 3 lands, the following can be verified with the current
walker:

1. **Side-by-side observer dump** — wire a recording observer into
   the downstream fxs plugin (behind an env flag, not committed),
   run it on a sample of modules with `ns_to_prefix_maps` currently
   matching the reference, and confirm the recorded
   `(source_module, kind)` event sequence is consistent with the
   reference's namespace order. This validates that the
   declaration-order DFS from steps 1–2 is correct for modules that
   do not exercise augments-out or constraint-source events.
2. **Unit test for transparent choice/case** — already covered by
   the existing `leafref_inside_case_resolves_sibling_through_transparent_choice`
   test. Confirmed in upstream `ea920e4`.
3. **Integration smoke test** — once a backend is wired (even
   experimentally), the 461-module reference bundle gives a
   single-number health metric: any *increase* below the current
   440/461 baseline indicates the walker contract is being violated
   for some module, and step 3 / step 5 can be planned with concrete
   per-module data via the existing `ns-map-classifier` tool.

The verification work happens downstream and does not require
upstream changes.

### Verification results (2026-06-03)

The verification described in §14 has been carried out: a recording
observer was wired into the fxs backend behind an env flag and run
against the full 461-module reference yangbundle. Results
(de-duplicated source-module set, own module excluded, comparing
walker output to the reference `ns_to_prefix_maps`):

| Outcome | Modules | Implication |
|---|---|---|
| Set + order match exactly | **281** | Steps 1–2 contract holds verbatim |
| Walker over-reports (extra-only) | 151 | Backend-side filter (modules already in `yang_header.imports`) — not a walker concern |
| Walker under-reports (miss-only) | 23 | Step 5 (§12 `on_constraint_source`) territory: annotation, deviation, obsolete-overlay modules |
| Combination | 6 | Steps 3 + 5 + filter |
| **Walker order diffs (set-matches only)** | **0** | The walker's encounter order is byte-exact for every module where membership is correct |

The zero-order-diff result is the strongest evidence the §5 visit-order
contract is correctly specified and implemented: across 281 real-world
modules, no spurious shuffle was observed. This justifies steps 3 and
5 advancing on the same ordering machinery; both are *additions* to
the event stream, not corrections to existing event order.

The miss-only cases break down into:

* `*-ann` modules (annotations contributing `when`/`must`): aaa-ann,
  acl-ann, dhcp-ann, ethernet-ann, native-ann, pnp-ann, pppoe-ann,
  spanning-tree-ann, atm-ann, voice-ann, openconfig-network-instance-l3,
  …
* `*-deviation`, `*-obsolete` overlays: cts-routing-deviation,
  eigrp-obsolete, ospf-obsolete, …
* Augment-out source modules (e.g. `Cisco-IOS-XE-native` is missing
  hsrp / interfaces / ip / ipv6 / line / location / logging /
  transport — all modules `native` augments INTO, contributing typed
  leafs at those modules' splice points).

The first two categories are the §12 use case verbatim. The third
category confirms §11 own-augment traversal is needed: `walk_own_tree`
alone cannot see leafs `M` contributes to other modules' trees.

### Updated migration prioritisation

1. **Step 5 first** (`on_constraint_source`) — fixes 23 miss-only
   cases. Smaller change (default-method trait extension + walker-side
   firing at `when`/`must`-bearing nodes), no compile-side changes.
2. **Step 3 second** (`walk_own_augments`) — fixes the augment-out
   subset of the 6 "both" cases plus the augment-out tail of the
   miss-only cases overlapping with augmenter modules. Larger change
   but no compile-side changes either: `CompiledModule::augments` is
   already populated.
3. **Backend filter** — drop walker-observed modules already in
   `yang_header.imports`. Pure downstream work; collapses 151 +
   filter-tail of 6 = up to 157 cases.

Total expected coverage if all three land: 281 + 23 + 151 + 6 = 461.
