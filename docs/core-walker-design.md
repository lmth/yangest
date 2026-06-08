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

## STATUS (2026-06-06): retired from core's critical path — read this first

The `SchemaWalker` and the encounter-order machinery described below are
**frozen and quarantined as an experimental, downstream-plugin concern.** They
are no longer extended, and byte-faithful emission is **not** a core goal. This
section records why; the rest of the document is preserved as the design of a
capability that may be revived *downstream*, but should not drive core decisions.

**Why this was retired:**

1. **The byte-identity requirement was validation-convenience, not a hard
   constraint.** No external system checksums the reference compiler's exact
   output bytes. Byte-diffing against the reference was simply the easiest way
   the (external, downstream) byte-faithful plugin author gained confidence the
   port was correct. **Normalized/semantic equivalence** — canonicalising the
   semantically-unordered sets (notably `ns_to_prefix_maps`) before comparing —
   satisfies that need without reproducing encounter order. That is now the
   intended validation path.

2. **The apparatus has zero in-tree consumers.** `SchemaWalker` /
   `TypeResolutionObserver` are referenced only by the walker module itself, its
   tests, and `bin/benches/walker_probe.rs`. The ordering-witness fields
   (`status_before_ext_meta`, `units_before_status`, `uses_grouping_statuses`)
   were written by the compiler but read by no emitter, and have been removed.
   None of the in-tree plugins (tree / yang / yang-expanded / yin / depend /
   swagger) use any of it.

3. **Byte-identity is the load that made the lazy model expensive — the
   §1↔§11 trade-off.** yangest's whole thesis is that a small per-module memory
   footprint lets it hold many modules resident and compile the dependency DAG
   in *parallel waves* (low memory ⟹ parallelism ⟹ speed). The lazy grouping
   model (`uses` stored as a reference, refine/deviate/annotation applied as
   overlays) is what keeps the footprint small. **Byte-identity (§1) requires a
   whole-tree, encounter-order traversal of the expanded forest; on a lazily
   expanded tree that means re-expanding via `SchemaNode::clone`, which is
   O(subtree)** — so the walk deep-clones shared groupings per descent
   (~440 s / 3.5 GB on a ~1071-module bundle; >10 GB and non-terminating once
   `uses`-body augments are expanded, see §11). The encounter-order requirement
   and the lazy model are in direct tension, and the walker is the seam where
   they grind. Retiring the requirement dissolves the tension and restores the
   memory/speed thesis. The reference compiler gets the order "for free" only
   because it expands eagerly and collects data as a side effect of one in-order
   build — a trade yangest deliberately does not make.

**Framing correction (the order is deterministic, not OTP-random).** Earlier
discussion described the reference order as "undefined but predictable for a
given OTP/Erlang version," implying runtime hash randomness. The evidence says
otherwise: the reference orders insertions **by source position** (see the
`AugmentEntry` doc-comment in `core/src/compiler/types.rs`). The order is
*deterministic but process-dependent* — the interleaving of visit pattern,
augment splicing, and typedef-chain walking. The three failed derivation
attempts (§1: alphabetical / reverse / DFS) failed because none of them sorted
by the source `Pos` the data model already carries. So if a hard byte-identity
need is ever revived **downstream**, prefer reconstructing the order by *sorting
carried source positions* over reviving the expensive live walk — it preserves
the fast lazy model. (Caveat: position-sort reconstructs node-visit order but
may not capture intra-node type-resolution order, e.g. innermost-base-type-chain
first; probe before betting on it.)

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
11. [Step 3 detail — own-augment traversal](#11-step-3-detail--what-augment-splicing-must-actually-do)
12. [Step 5 — `on_constraint_source` (DONE)](#12-step-5--whenmust-source-module-exposure-done)
13. [Open questions](#13-open-questions-revised)
14. [Verification plan + results](#14-verification-plan-for-the-partial-implementation)
15. [Step 6 — `on_extension_attached`](#15-step-6--on_extension_attached-event)

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

### Step 4 implementation specification

For a maintainer picking up step 4:

* **Cargo feature**: add `ref-data = []` to `core/Cargo.toml`. Tests
  guarded with `#[cfg(feature = "ref-data")]` so default `cargo
  test` skips them. CI can opt in with `--features ref-data`.
* **Fixture layout**: `core/tests/ref-data/` holds the YANG sources
  (a curated slice of ~15 modules from a real reference yangbundle —
  see *suggested slice* below) plus a `expected/` subdirectory with
  `<module>.ns_to_prefix_maps.json` files: the de-duplicated
  source-module sequence the walker should emit for each fixture
  module, when driven by a reference observer that records typedef +
  leafref + constraint-source events.
* **Suggested slice** (chosen to exercise every walker code path with
  minimal LOC):
    * One simple typedef-only module (e.g. an `*-types` module).
    * One module with a leafref crossing into another module.
    * One module with augments out into a host module (exercises
      step 3).
    * One annotation module + its target module (exercises §6 +
      step 5 + step 6).
    * One module with a deviation injecting a `when` into another
      module's node.
    * Two modules that exhibit the pure-2-element-reverse and the
      complex-shuffle patterns observed in real bundles.
* **Test harness**: `core/tests/ref_parity.rs` reads each
  `expected/<module>.ns_to_prefix_maps.json`, parses fixtures with
  `parse_yang` + `compile_module` (or constructs a
  `ModuleRegistry` from the slice), instantiates a recording
  observer, drives `SchemaWalker::walk` per module, dedups the
  observer's events into `(source_module)` order, and asserts
  equality against the expected JSON.
* **Diff output**: on failure, print the longest common subsequence
  of expected vs actual so the maintainer can see "missing X here,
  extra Y there" without re-deriving by eye. Use `similar` or
  hand-rolled LCS — keep it dependency-light.
* **Updating expectations**: a `REGEN_REF_PARITY=1` env flag should
  rewrite the JSON files from the current walker output, so updating
  the slice doesn't require manual JSON editing. CI runs without the
  flag set.

---

## 10. Migration plan

### Status (as of 2026-06-03)

| Step | Description | Status |
|---|---|---|
| 1 | `core::walker` skeleton | **Done** (`ea920e4`) |
| 2 | Wire observer firing at resolution sites | **Done** (`ea920e4`) |
| 3 | `walk_own_augments` (own-augment traversal) | **Done** (`d3a5cf5`); Uses-body follow-up **Reverted** (the obvious approach in `84da49f` regressed runtime — see §11); annotation source-module attribution **Done** (`ExtensionInstance::injection_source_module`) |
| 4 | `ref-data`-gated parity tests | **Pending** — see §9 |
| 5 | `on_constraint_source` for `when`/`must` | **Done** (`53062f7`) |
| 6 | `on_extension_attached` for foreign-module extensions | **Done** (`813bd2f`) |

Only step 4 (the `ref-data`-gated reference-parity tests) and the
§11 Uses-body follow-up remain. Step 4 is insurance against future
regressions and needs checked-in reference outputs; the Uses-body
follow-up is documented in §11 with three candidate paths
forward — the natural approach was tried in `84da49f` and reverted
because of catastrophic runtime regression.

### Original step descriptions

This was upstream-only design work; implementation proceeds in
independently mergeable commits:

1. **Add `core::walker` module skeleton** — `SchemaWalker`,
   `WalkOptions`, `walk` and `walk_with_visitor` methods. The walker
   compiles and runs but visits in declaration order *without* firing
   observer events at type-resolution sites. Existing tests pass; no
   backend uses it yet.
2. **Wire observer firing at resolution points** — `walk` calls
   `types.resolve_with_observer` at each node with a type. Add the
   unit and observer-event tests from §9.1 and §9.2. No backend
   migrated yet.
3. **Augment-splicing and deviation-inlining ordering** — see §11.
4. **Reference-parity tests** — see §9.
5. **`on_constraint_source` event** — see §12.
6. **`on_extension_attached` event** — see §15.

After steps 3, 4, 6 land the upstream is fully ready for
byte-faithful backends. The downstream migration (rewriting the
backend's `build_ns_to_prefix_maps` against `SchemaWalker`) happens
separately and does not touch upstream further.

---

## 11. Step 3 detail — what augment splicing must actually do

**Status: implemented.** `SchemaWalker::walk` now runs `walk_own_tree`
followed by `walk_own_augments`; both are also public for fine-grained
use. `walk_own_augments` positions a cursor at each augment target in the
host (via `Cursor::reroot` + `follow_path`) and, for each node the
augment contributes, visits the matching *host-side* child (read through
the new `Cursor::child_nodes`) so host-applied annotations are surfaced
(the option-1 / cursor-based-descent approach below). `if-feature`-disabled
augments fire no events. The six tests below are in
`core/src/walker/tests.rs`. The original spec is preserved for rationale.

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
already finalising per-module output must complete
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

### Step 3 implementation specification

For a maintainer picking up step 3 cold, the following is the full
spec.

**Data sources** (already populated by `compile_module`):

* `CompiledModule::augments: Vec<AugmentEntry>` — the augments this
  module declares into other modules' trees.
* `AugmentEntry::target_path: SchemaPath` — `Vec<PathStep>`
  with `{prefix, name}` per step. The first step's prefix is the
  module's own prefix or an imported prefix; resolve via
  `module.prefix_map`.
* `AugmentEntry::nodes: Vec<SchemaNode>` — children to splice at the
  target. These nodes already carry the correct `module_name`
  (= the augmenting module) and have their `when`/`musts` populated.
* `AugmentEntry::when` / `if_features` — augment-level guards. An
  augment skipped by an `if-feature` failure should fire **no**
  observer events. Use `crate::compiler::is_feature_enabled` to
  evaluate.
* `AugmentEntry::pos` — source position. Iterate `module.augments`
  in `Vec` order (which is source-declaration order); the reference
  compiler does not re-sort.

**Iteration order**: `module.augments.iter()`, then for each augment
the body in `nodes.iter()` declaration order. No alphabetical sort,
no target-path sort.

**Cursor positioning**:

```rust
fn cursor_for_augment_target(
    target_path: &SchemaPath,
    module: &CompiledModule,           // augmenting module (for prefix_map)
    registry: &ModuleRegistry,
    ctx: &ExpansionCtx<'_>,
) -> Option<Cursor<'_>> {
    let first = target_path.first()?;
    let host_name = first.prefix.as_deref()
        .and_then(|p| module.prefix_map.get(p))
        .or(Some(&module.key.name))?;
    let host = registry.resolve_import(host_name, None)?;
    let mut cur = Cursor::root_of(&host, ctx);
    let axes: Vec<Axis> = target_path.iter()
        .map(|step| Axis::Child(QName {
            module: Some(/* resolved module for step.prefix */),
            name: step.name.clone(),
        }))
        .collect();
    cur.follow_path(&axes).ok()?;
    Some(cur)
}
```

If the cursor cannot be positioned (e.g. host module not loaded,
target node missing), skip that augment silently and continue.
Backends asking for byte-faithful output can detect this via the
existing diagnostics path; the walker is best-effort and never
panics on schema gaps.

**Choice/case targets**: an augment may target a `case` inside a
`choice`. The same transparent-choice/case logic the existing
`visit_node` uses applies — pass the augment's children to
`visit_node` with `data_cursor` positioned at the enclosing data
node, not the case.

**Recursive bodies / `uses grouping;`**: ⚠️ *Open follow-up; the
obvious fix is unsafe at scale.* An augment body of the form
`augment X { uses g; }` stores `aug.nodes` as a single
`SchemaNode` of `kind::Uses` whose `name` is `"__uses__"`, **not**
the grouping's expanded leaves. Two pitfalls follow:

* **`body_node.children(ctx)` returns nothing** when called directly
  on the Uses node. Uses are expanded by their *parent's*
  `expand_children` call, not by descending into the Uses itself.
* **Host-tree `child_nodes()` does not show expanded leaves either**
  for foreign-augment contributions: `Cursor::augment_children`
  currently extends with `aug.nodes.iter().cloned()` — i.e. the raw
  Uses node — so cursoring at the augment target sees
  `Uses{name:"__uses__"}` on the cursor's child list.

The simple "match `aug.nodes[i].name` against `host_children` by
name" loop therefore visits *zero* nodes for any Uses-shaped
augment body — empirically the dominant pattern in real-world
yangbundles, where almost every augment body uses the grouping
idiom.

**Resolution attempted (commit `84da49f`, reverted in
`<this commit>`):** the natural fix — make
`Cursor::augment_children` and the walker's body iteration call
`compile::expand_children` on `aug.nodes` — is *correct* on a
small fixture but **scales catastrophically on heavy bundles**.
The probe `walker.walk` per module climbs from a few minutes to
runaway memory growth (>7 GB and still rising at termination) on
the reference yangbundle (~461 modules, deep augment-target paths).
Two amplification factors compound:

1. **`augment_children` is invoked on every cursor descent**
   (`Cursor::current_children` is on the path of `find_child`,
   `child_nodes`, and augment-target navigation). The walker
   descends a lot.
2. **`expand_children_inner` clones every node** of an
   `expand_uses_lazy`-cached body into the returned `Vec`. The
   grouping cache short-circuits `expand_uses_lazy` itself, but
   the per-call `Vec::clone` is O(grouping_size) in deep
   `SchemaNode` clones (each carries `pmap`, `extensions`,
   `Vec<WhenExpr>`, etc.). With many matching Uses-shaped
   augments per descent, allocator pressure dominates.

A targeted "fast-path-clone unless aug.nodes contains a Uses"
optimisation was tried but did not help materially.

**Status (2026-06-06): RETIRED / won't-fix in core.** This follow-up only
matters for byte-faithful encounter-order emission, which has been retired from
core (see the STATUS banner at the top of this document). It is not pursued. The
candidate paths below are preserved only for a possible *downstream* revival; do
not invest core effort here. The diagnosis remains accurate and is the reason
the requirement was dropped rather than chased: the real cost is the O(subtree)
`SchemaNode::clone` in the cursor's per-descent `current_children`, which the
byte-identity walk hits hardest.

*(Historical) three candidate paths, ranked by perceived robustness:*

1. **Cache expanded augment bodies once per
   `(AugmentEntry, ExpansionCtx)`** via a new per-`ExpansionCtx`
   cache keyed on `*const AugmentEntry` (or interior-mutable
   storage on `CompiledModule`). `Cursor::augment_children` then
   `Arc::clone`s the cached `Vec` instead of re-walking it.
   Per-call cost in `current_children` becomes O(N) in result
   size, not O(N × matches × per-call expansion).
2. **Compile pre-expands augment bodies** so `aug.nodes` always
   stores post-grouping leaves. One-time cost at compile. Care
   needed to keep refines / local augments / when / if-feature
   intact at re-expansion.
3. **Keep the cursor raw, push synthetic frames in the walker**
   for visited body leaves — bypass `find_child` entirely for
   the per-augment-body iteration.

Path (1) is the most defensive; (3) is closest in spirit to the
host-tree-iteration rule the upstream tests pin. All need fresh
heavy-bundle bench numbers before declaring done.

**Path (1) empirically ruled out (measured, not landed).** Path (1)
was implemented (a per-`ExpansionCtx` `expand_augment_body` cache keyed
on `*const AugmentEntry`, with both `Cursor::augment_children` and the
walker's body iteration `Arc::clone`-ing the cached expansion) and run
through `bin/benches/walker_probe` — a parallel, bundle-mode walk of a
representative ~1071-module vendor bundle. It **reproduces the
regression**: RSS climbs past 10 GB and never completes. The cache
removes the *re-expansion*, but not the per-call **deep clone** of the
expanded body: `Cursor::current_children` rebuilds an owned
`Vec<SchemaNode>` on *every* `find_child`/`child_nodes` call, and the
walker calls those O(children) times per node. The baseline walk (no
Uses expansion at all) already costs ~440 s / ~3.5 GB peak on that
bundle for exactly this reason — expanding the augment bodies only
amplifies a cost that is already in `current_children`.

**Implication.** The real fix is not "cache the augment body" — it is
"stop `current_children` from deep-cloning per descent." Candidates:
memoize `current_children` by absolute path in the `ExpansionCtx`
(bounded by tree size, but the fully-expanded forest is itself large —
needs its own bench), or restructure the cursor to share subtrees by
`Arc` instead of cloning. This is a cursor-level performance redesign,
larger than a localized augment helper, and must be validated with
`walker_probe` against the heavy bundle before landing.

The §14 verification table (post step 3) still applies: even
without the Uses-body fix, step 3 unblocks two cases. The dominant
17 Uses-body misses are blocked on this follow-up; do not assume
they are easy — and the obvious cache (path 1) is *not* the answer.

**Cross-module augment chains**: when module A augments into B, and B
augments into C, A's walk visits A's augment into B *only*. B's augment
into C is B's responsibility, fired during B's own walk. This
matches the reference compiler's per-module compilation model.

**Suggested implementation skeleton**:

```rust
pub fn walk_own_augments<O: TypeResolutionObserver>(&self, observer: &mut O) {
    for aug in &self.module.augments {
        if !self.augment_is_enabled(aug) {
            continue;
        }
        let Some(target_cursor) = cursor_for_augment_target(
            &aug.target_path, self.module, self.registry, self.ctx
        ) else { continue };

        // Augment-level when/musts also need on_constraint_source;
        // they fire once per augment, with the augmenting module as source.
        for w in &aug.when {
            // The "node" we report is the first child the augment splices,
            // because the augment statement itself has no SchemaNode.
            // Backends that care about exact attribution can look at the
            // node's ancestry. (Open question — see §13.)
        }

        for child in &aug.nodes {
            self.visit_node(child, &target_cursor, observer, &mut |_| {});
        }
    }
}

fn augment_is_enabled(&self, aug: &AugmentEntry) -> bool {
    aug.if_features.iter().all(|f|
        crate::compiler::is_feature_enabled(self.ctx, &self.module.key.name, f)
    )
}
```

The `walk_with_visitor` variant follows the same pattern but threads
the visitor through.

#### CRITICAL: source nodes vs host-tree nodes

The skeleton above iterates `aug.nodes` (the augment-body
source-declaration nodes stored on the augmenting module). That
form is convenient and is correct for `on_typedef_resolved` /
`on_leafref_resolved` events because types and leafref paths are
copied verbatim into the source nodes during compile.

It is **not** correct for `on_extension_attached` events when the
host module's compile pipeline applies `apply_annotations`. Path-
based annotations (`acme:annotate "/host:x/host:y/aug:z"`) are
merged into the **host's compiled SchemaNode tree** at the splice
point, not back into `aug.nodes`. Walking `aug.nodes` therefore
misses those foreign extensions even though, logically, they
attach to nodes whose `module_name` is the augmenting module.

Empirical evidence: in a 461-module reference bundle, every
`*-ann` annotation module that targets a node grafted via an
augment-out exhibits this — a probe walking only `aug.nodes`
reports `walker_seq=[]` for the augmenting module, while the
reference compiler's `ns_to_prefix_maps` for that module includes
the annotation source.

**Resolution**: when walking own-augments, prefer
**iterating the host's compiled subtree at the cursor** rather
than the augmenting module's `aug.nodes`. Two equivalent options:

1. **Cursor-based descent** (recommended): after positioning the
   cursor at the augment target in the host, enumerate the
   host-side children whose `module_name == self.module.key.name`
   and call `visit_node` on each. This naturally surfaces any
   annotations the host applied during its compile.
2. **Apply annotations to `aug.nodes` at compile time**: extend
   `apply_annotations` so that when a target path navigates into
   a foreign-augment-owned subtree, the resulting extensions are
   *also* recorded on the augmenting module's `aug.nodes`. The
   walker can then keep its simpler `aug.nodes`-iteration form.
   This is more invasive (compile-side change) but keeps the
   walker free of host-tree lookups.

For the maintainer: option (1) is the smaller change and lives
entirely in `walker/`. Option (2) is preferable if other future
plugins need annotation-aware augment-body access outside the
walker. Pick (1) unless a second use case appears.

**Tests to add** (`core/src/walker/tests.rs`):

1. `own_augments_fire_in_declaration_order` — module M with two
   augments into module H; assert events from M's walk include
   leafs from both, in source order.
2. `own_augments_skipped_if_feature_disabled` — augment guarded by
   `if-feature foo` where `foo` is disabled; assert no events.
3. `own_augments_position_cursor_at_target` — augment target is
   nested under a container in the host; assert a leafref in the
   augment body resolving via `..` finds a sibling in the *host*
   (not the augmenting module).
4. `cross_module_augment_chain_per_module_attribution` — three
   modules A, B, C where A augments B, B augments C; walking A
   fires events only for A's augment; walking B fires events for
   B's own tree + B's augment into C; walking C fires events for
   its own tree.
5. `walk_calls_own_tree_then_own_augments` — assert event order
   from `walk()` equals `walk_own_tree()` events followed by
   `walk_own_augments()` events.
6. `own_augments_surface_host_applied_annotations` — module M
   augments host H at `/H:root` with leaf `M:x`. After M is
   compiled, simulate H applying `apply_annotations` injecting a
   foreign extension `acme:callpoint` onto `H:root/M:x`. Walking
   M must fire `on_extension_attached("acme-ext", _, _)` for
   `M:x` exactly once. This pins the cursor-based-descent
   semantics described above and prevents accidental regression
   to `aug.nodes`-only iteration.

**Bundle-level smoke-test target**: with step 3 in place, a byte-faithful
backend (using the `YANGEST_NS_DUMP=1` probe in the downstream
experiments tree) should:

* Drop *miss-only* from the post-step-5/6 baseline (19 modules) to
  **near zero** — the remaining miss-only entries after step 6
  landed are dominated by `*-ann` annotation modules whose foreign
  extensions live on host trees, exactly the case test 6 above
  pins down. Verified via a probe sweep on 2026-06-03 against the
  reference yangbundle.
* Drop "both" from 5 → 0.
* "extra-only" is unaffected by step 3 (it needs the backend's
  imports filter).

---

## 12. Step 5 — when/must source-module exposure (DONE)

**Status: implemented in commit `53062f7`.** `ConstraintKind { When,
Must }` and `TypeResolutionObserver::on_constraint_source` are
upstream and synced; `SchemaNode::musts()` accessor added; walker
fires when-then-must events at each visited node before type
resolution. Three tests added; full suite (45 integration + 80
core unit) passes.

The original specification of this step is preserved below for
historical reference and to document the design rationale.

---

Section 6 of the spec deferred exposing `when`/`must` source modules
because the trait-extension cost was uncertain. Empirical evidence
from a 461-module reference yangbundle now refutes that deferral: of
21 modules whose `ns_to_prefix_maps` differs from the reference,
**three have wrong membership** — meaning typedef+leafref events
alone cannot reproduce the reference set. The missing signal is the
source module of `when`/`must` constraints contributed by deviations
and annotations applied to nodes during compilation.

Concretely, for one large device module the reference includes a
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

This event is exactly what a byte-faithful backend's existing
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
  consumer (ns-map building) does not need pruning; the `tree`
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
   the downstream backend (behind an env flag, not committed),
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
observer was wired into the downstream backend behind an env flag and run
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

* Annotation (`*-ann`) modules contributing `when`/`must` constraints.
* `*-deviation` / `*-obsolete` overlay modules.
* Augment-out source modules (e.g. a device's root module is missing the
  several host modules it augments INTO — all contributing typed leafs at
  those modules' splice points).

The first two categories are the §12 use case verbatim. The third
category confirms §11 own-augment traversal is needed: `walk_own_tree`
alone cannot see leafs `M` contributes to other modules' trees.

### Verification results (2026-06-03 — post step 6)

After step 6 (`on_extension_attached`) landed, a re-run of the same
verification shows the foreign-extension event is now firing — 144
modules' `walker_seq` includes the shared annotation-extension module
from `acme:callpoint`/`acme:hidden`/etc instances. But the post-step-6
counts are:

| Outcome | Pre step 6 | Post step 6 |
|---|---|---|
| Set + order match | 281 | 175 |
| Extra-only | 151 | 262 |
| Miss-only | 23 | 19 |
| Both | 6 | 5 |

The drop in set-matches and rise in extra-only is **expected**: step 6
now reports many shared-annotation-module events the reference compiler
already filters out (most modules import that shared extension module, so
the backend's import-filter pass drops it). Steps 1/2/5/6 are correct;
the *content* is now strictly richer, and the residual difference
between walker output and the reference is explainable by the
backend-side filter alone (or, for the 19 miss-only, by step 3 not
yet being implemented).

**Persistent miss-only after step 6**: spot-checked against the
`*-ann` targets, the remaining miss-only is dominated by the
augment-into-host pattern. For example, an annotation module `A-ann`
declares `acme:annotate "/host:root/host:a/A:server/A:key"`.
The leaf `key` was contributed to the host's tree by `A`'s own
augment-out; its `module_name` is `A`. When the host
is compiled, `apply_annotations` decorates the host-tree node at
`key` with `acme:callpoint`. When the walker walks
`A` from its root, `A`'s `module.children(ctx)`
does NOT include `key` (it lives under the host's root, not A's).
Probe result: `walker_seq=[]` for `A`. Reference compiler's
`ns_to_prefix_maps` for A: `["A-ann", "A"]`.

This empirically confirms what §11 documents: step 3
(`walk_own_augments`) is the required unblocker, and the
`own_augments_surface_host_applied_annotations` test pins the
cursor-into-host-tree semantics that surfaces these annotations.

### Verification results (2026-06-03 — post step 3)

Step 3 (`walk_own_augments`) landed in `d3a5cf5`. Re-running the
probe shows only modest movement:

| Outcome | Pre step 3 | Post step 3 |
|---|---|---|
| Set + order match | 175 | 174 |
| Extra-only | 262 | 264 |
| Miss-only | 19 | 17 |
| Both | 5 | 6 |

Two miss-only cases were unblocked (a few flipped between
categories). The remaining 17 miss-only follow two patterns:

1. **Uses-shaped augment bodies** are *not* visited. Spot-check on
   a representative device module: walker_seq=`[]` despite it having
   several augments into a host module. Its annotation module
   annotates leaves under those splice points. Each augment's body is
   `uses <grouping-name>;` rather than direct leaf statements, so the
   step-3 implementation's `aug.nodes`-name-match-against-
   host-children loop visits zero nodes per augment (Uses node's
   name is the grouping name, no host-side counterpart by that
   name). See §11 "Recursive bodies / `uses grouping;`" for the
   three candidate paths forward. *(Attempted in `84da49f` and
   reverted because of catastrophic runtime regression on heavy
   bundles; remains an open follow-up.)*
2. **Annotation source-module attribution**: even when step 3
   reaches the host-tree node carrying a foreign extension, the
   walker fires `on_extension_attached(ext.module, …)` where
   `ext.module` is the **definition** module of the extension
   (the shared annotation-extension module), not the **annotation
   source module** that injected it (e.g. an `A-ann` overlay).
   The reference compiler's `ns_to_prefix_maps` records the
   latter.

   Probe confirms: post-step-3 `walker_seq` for a device module
   `A` is `["<shared-ext-module>"]` (was `[]`); the reference is
   `["A-ann", "A"]`. Step 3's host-tree iteration is working — but
   the source-module reported is wrong for annotation-injected
   extensions.

   **Resolution path** *(implemented)*: `ExtensionInstance` carries
   a separate `injection_source_module: Option<String>` field,
   populated during `apply_annotations` from `ann.from_module.name`
   (path-based) and at extension-collection time from
   `ast_ann_index.module_key_for_file(ext.pos.orig_file())`
   (AST-based). The walker reports `ExtensionInstance::source_for_ns`
   (which prefers `injection_source_module` over `ext.module`) when
   firing `on_extension_attached`, and uses it for the foreign-source
   filter too. An observer can now recover the annotation source from
   a finalised `SchemaNode`.

Annotation source-module attribution is implemented (see §10
status table); the Uses-body fix remains an open follow-up.

#### Empirical effectiveness of follow-up (b) — open question

After `57cdaa4` (Gap 2 fix) landed, the bundle-scale probe was
re-run on 461 modules. The three new walker tests pass, but the
expected unblocking of `*-ann` modules in real-world output is
not yet seen:

| Module | walker_seq (post-revert, post-Gap-2) | reference |
|---|---|---|
| `A` | `["<shared-ext-module>"]` | `["A-ann", "A"]` |
| `B` | `[]` | `["B-ann", "B"]` |
| `C` | `[]` | `["C-ann", ..., "C"]` |
| `D` | `[]` | `["D-ann", "D"]` |

Of the 6 "both" cases (mismatched set + order), 4/6 have
`extra=['<shared-ext-module>']` paired with `missing=['<X>-ann']`. That
indicates the walker fires `on_extension_attached` for the annotation
extensions but with `source = '<shared-ext-module>'` — i.e.
`source_for_ns()` returns `ext.module` because
`injection_source_module` is `None` at those instances.

The annotation-target topology of these modules is the likely
explanation. Example: an annotation module `A-ann` annotates paths
like `/host:root/host:mac/A:access-list` — i.e. nodes that
module `A` augments INTO the host's tree. The annotations are
therefore applied to nodes contributed via `A`'s *cross-module
augment*. Two hypotheses for why `injection_source_module` is `None`
at those sites:

1. **Compile ordering**: `apply_annotation_to_node` (compile.rs
   ~3519) sets `injection_source_module` correctly when called,
   but it isn't called for the augment-spliced child contributed
   by `A` into the host (the splice point may not run the
   apply-annotations pass that resolves
   `acme:annotate "/host:root/host:mac/A:access-list"`).
2. **AST-overlay path returns None**:
   `ast_ann_index.module_key_for_file(sub.pos.orig_file())`
   doesn't recognise `A-ann.yang` as an annotation-source file in
   this code path, falling back to `ext.module = "<shared-ext-module>"`.

A focused investigation by the maintainer is needed; the
plumbing on the walker side is correct (`source_for_ns()` is
called), so the gap is in **populating
`injection_source_module`** for the augment-spliced extension
sites, not in surfacing it.

Suggested investigation steps:
* Add a temporary `eprintln!` in `apply_annotation_to_node` and
  the AST-overlay path to confirm whether they run for one of the
  affected modules (e.g. `A-ann` injecting into `A`'s augment
  children).
* Cross-check whether `acme:annotate "/p1:x/p2:y/.../p1:z"`
  paths are resolved against the compiled-with-augments host
  tree (so the spliced `A` child is visible) or the bare host
  declaration.

### Updated migration prioritisation

1. **Step 5 first** (`on_constraint_source`) — DONE in `53062f7`.
2. **Step 6 second** (`on_extension_attached`) — DONE in `813bd2f`.
   Confirmed via probe: 144 modules now report foreign-extension
   events from the shared annotation-extension module that were
   previously invisible.
3. **Step 3 third** (`walk_own_augments`) — DONE in `d3a5cf5`,
   plus follow-up (b) **DONE**
   (`ExtensionInstance::injection_source_module` so
   `on_extension_attached` reports the annotation-source module,
   not the extension-definition module). Follow-up (a) — Uses-body
   expansion — was attempted in `84da49f` and reverted because of
   catastrophic runtime regression on heavy bundles; **open** with
   three candidate paths in §11.
4. **Backend filter** — drop walker-observed modules already in
   `yang_header.imports`. Pure downstream work; collapses the
   ~264 extra-only to near zero.

Total expected coverage if step 3 follow-ups + filter land: 461/461
on the reference bundle.

---

## 15. Step 6 — `on_extension_attached` event

**Status: implemented.** `TypeResolutionObserver::on_extension_attached`
is upstream; the walker fires it in `visit_node` after the
`on_constraint_source` loop and before type resolution, once per
foreign-module extension instance (`ext.module != node.module_name`) in
`node.extensions` declaration order. The four tests from the *Tests*
subsection below are in `core/src/walker/tests.rs`. The original
specification is preserved below for design rationale.

### Why this step is needed

The walker probe (§14) classified 23 modules as miss-only after
step 5 landed. Inspection showed two distinct causes:

1. The augment-out tail (a device's root module whose typed leafs live
   under hosts the walker never sees from that root). Step 3 (§11)
   addresses this.
2. Annotation modules (`*-ann`) that attach **extension instances**
   such as `acme:callpoint`, `acme:hidden`, `acme:dependency`,
   etc. to nodes in a target module. These are NOT `when`/`must`
   constraints — they're orthogonal extension attachments. They
   are observable at `SchemaNode::extensions` after the annotation
   pass merges them in, but the walker has no event surface to
   notify observers when a foreign-module extension is attached
   to a visited node.

Empirically, 14 of the 23 miss-only cases on the reference bundle
are exclusively in this second category (various `*-ann` annotation
modules). Without step 6 a backend cannot reach byte-faithful output
for any module that imports an annotation overlay.

### Trait surface (proposed)

Default-method extension on `TypeResolutionObserver`, mirroring the
shape of `on_constraint_source`:

```rust
pub trait TypeResolutionObserver {
    // ...existing methods...

    /// Called once per foreign-module extension instance attached
    /// to `node`, in declaration order within `node.extensions`.
    /// Fired *after* `on_constraint_source` events at the same
    /// node and *before* type resolution + child descent.
    ///
    /// `source_module` is `ext.module` — the module that declared
    /// the extension instance. By contract, this method only fires
    /// when `ext.module != node.module_name`; observers do not need
    /// to filter same-module extensions.
    fn on_extension_attached(
        &mut self,
        _source_module: &str,
        _ext: &ExtensionInstance,
        _node: &SchemaNode,
    ) {}
}
```

`ExtensionInstance` already exists in `core/src/compiler/types.rs`
and carries `module: String` (the source module of the extension
instance, not necessarily the module of the extension *definition*),
`name: String`, `arg: Option<String>`, and substatement information.
No new data needed.

### Walker-side firing rule

In `SchemaWalker::visit_node`, after the `on_constraint_source`
loop and before `resolve_node_type`:

```rust
for ext in &node.extensions {
    // `source_for_ns()` is the injection-source module if the extension was
    // annotation-injected, else its defining module (follow-up B).
    let source = ext.source_for_ns();
    if source != node.module_name {
        observer.on_extension_attached(source, ext, node);
    }
}
```

The same-module filter is intentional: backends consuming this
event for `ns_to_prefix_maps` only care about *foreign* sources.
A backend that needs to see same-module extensions should walk
`node.extensions` directly — that's a node-property query, not an
encounter-order event.

### Order guarantees

* Within a node: `node.extensions.iter()` is the declaration order
  the annotation merge produces. Stable across runs.
* Between nodes: standard DFS encounter order, identical to all
  other events.
* Relative to other events at the same node:
  `on_constraint_source` first, then `on_extension_attached`,
  then `on_typedef_resolved` / `on_leafref_resolved`.

### Tests

In `core/src/walker/tests.rs`:

1. `fires_on_extension_attached_for_foreign_module_only` — build
   two modules `A` and `B`. `A`'s leaf `x` has its own-module
   extension `A:hint` and a foreign-module extension
   `B:callpoint`. Walking `A` fires exactly one
   `on_extension_attached("B", _, _)` event for `x`.
2. `fires_extensions_in_declaration_order` — leaf with three
   foreign-module extensions, attached in order `B:a`, `C:b`,
   `B:c`. Assert events arrive in exactly that order, source
   modules `["B", "C", "B"]` (no per-source dedup at the walker
   layer — that's the observer's job).
3. `default_observer_method_means_no_extension_events` — trait
   default method does nothing; an observer that doesn't override
   it sees zero `on_extension_attached` calls (verified via a
   recording observer that logs every method call).
4. `fires_after_constraint_before_resolution` — leaf with a `when`
   from foreign module `B` AND an `acme:callpoint` from foreign
   module `C` AND a leafref to foreign module `D`. Recording
   observer: assert event order is `on_constraint_source(B,
   When, _)`, `on_extension_attached(C, _, _)`,
   `on_leafref_resolved(_, target in D)`.

### Out of scope

* Same-module extension events. Backends wanting them can walk
  `node.extensions` directly.
* Extension-substatement traversal. If an extension carries
  substatements that themselves contain `when` or `must`
  constraints, those are a separate question; current backends
  don't need them.
* Per-extension hash-key uniqueness or dedup. The walker fires
  once per occurrence in `node.extensions`; observers that want
  uniqueness use an `IndexSet` keyed on whatever they care about.

### Observed metric movement (post-implementation)

Probe sweep on 2026-06-03 with step 6 implemented:

* Miss-only: 23 → 19. Step 6 unblocked 4 of the *-ann cases (those
  whose annotations target paths landing on the annotation source
  module itself, not on host-tree augment splice points).
* Set-match: 281 → 175 (drop expected — step 6 now reports many
  shared-annotation-module foreign-extension events the backend's
  import filter would suppress).
* Extra-only: 151 → 262 (rise expected — same reason).
* Both: 6 → 5.

Step 6 is contract-correct; the residual miss-only of 19 is the
augment-into-host *-ann pattern documented in §14, which step 3
addresses. After step 3 lands with cursor-into-host-tree
semantics (§11), miss-only is expected to reach 0.

The backend-side import filter, applied on top of the walker
output, then collapses the 262 extra-only to near zero.
