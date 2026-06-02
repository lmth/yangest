# Missing Required Features in yangest-core

This document inventories functionality that backends (downstream plugins) commonly
need but that yangest-core does not currently provide. It was compiled while
implementing a binary-compatible `fxs` backend whose reference output is produced
by `confdc` (which is built on top of yanger). The same gaps recur for any
backend that needs to reason about resolved leafref targets, cross-module schema
references, or output ordering that mirrors a specific reference compiler.

The features below are listed in roughly descending order of leverage: the
earlier ones unblock or simplify several of the later ones.

---

## 1. Stateful schema-tree cursor

### What it is

A stateful navigator over the *expanded* schema tree, with operations like:

- `new(root_module)`
- `move(cursor, axis, qname) -> Result<Cursor>`
- `find_child(cursor, qname) -> Option<Cursor>`
- `follow_path(cursor, parsed_path) -> Result<Cursor>`
- `reset(cursor)`, `reset_to_root`, `reset_to_init`
- `ancestors(cursor) -> Vec<&Sn>`
- `floating_cursor` — a cursor that is not anchored to a single tree position
  but tracks the set of possible positions (used while validating relative
  XPath expressions)
- `set_xpath_root(cursor, ...)` — pin a root for relative-path resolution
- `quick_move` — a faster `move` that skips some validation when the caller
  has already proven the step is safe

In yanger, this is `yang_cursor.erl` (≈ 520 lines). It is the primary mechanism
for cross-node operations: leafref target lookup, when/must dependency
extraction, augment target lookup, deviation target lookup, and validation of
key references in keypaths.

### Why backends need it

Without a cursor, every backend reimplements path traversal against
`SchemaNode` trees by hand. That has three knock-on effects:

1. **Bugs are not centralised.** Cursor-aware operations such as
   "skip transparent `case` and `choice` nodes when computing relative
   sibling paths" must be re-derived in each backend.
2. **Leafref/when/must dependency tracking is brittle.** Each backend ends up
   building its own ad-hoc resolver, often with subtle inconsistencies relative
   to the canonical (yanger) behaviour.
3. **There is no shared notion of "current position".** Plugins that need to
   emit per-node deps (e.g. compiled XPath expressions with cursor-relative
   keypaths) cannot share helpers with the rest of the compiler.

### Concrete failures observed without it

- "Sibling reference inside `case` becomes `[..,<unknown>]` instead of `[.]`"
  in dependency keypaths — the cursor is what makes `case` transparent.
- "Leafref target type cannot be resolved" — without a cursor there is nothing
  to walk the parsed leafref path with.
- "Augment target lookup fails for nodes inside `uses`" — needs cursor that
  understands grouping expansion.

### Suggested API shape (Rust)

```rust
pub struct Cursor<'a> { /* opaque */ }

impl<'a> Cursor<'a> {
    pub fn new(root: &'a Module, ctx: &'a ExpansionCtx<'_>) -> Self;
    pub fn root_of(module: &'a Module, ctx: &'a ExpansionCtx<'_>) -> Self;
    pub fn current(&self) -> Option<&'a SchemaNode>;
    pub fn axis(&self) -> Axis;
    pub fn move_step(&mut self, step: &Step) -> Result<(), CursorError>;
    pub fn find_child(&self, qname: &QName) -> Option<Cursor<'a>>;
    pub fn follow_path(&mut self, path: &CompiledPath) -> Result<(), CursorError>;
    pub fn ancestors(&self) -> impl Iterator<Item = &'a SchemaNode>;
    pub fn reset_to_root(&mut self);
    pub fn floating(self) -> FloatingCursor<'a>;
}
```

---

## 2. Leafref target resolution

### What it is

Given a parsed leafref path and a starting cursor, walk the path to the target
schema node and return both the target node and its fully-resolved type
(including the entire chain of typedefs and restrictions). In yanger this is
`yang_types:follow_leafref_path/3,4` (≈ 200 LoC plus the surrounding type
machinery).

### Why backends need it

The FXS format stores a leafref-typed leaf as a chain of derivations whose
*base* is the resolved target type (e.g. a string with a length restriction).
Without target resolution, yangest can only emit `primitive_type = leafref` and
`{list, undefined}` for `leaf-list` targets — both of which prevent the
generated FXS file from matching the reference.

The same need arises for any backend that wants to:

- emit type metadata that flattens leafrefs (CLI, JSON-schema, OpenAPI, …)
- validate a default value against the *target's* restrictions
- compute key-reference dependencies between lists

### Required pieces

1. A type registry that can resolve any user-typedef or built-in type to its
   complete `Type` description (`yang_types:lookup_type/2`).
2. A cursor (see #1) that can walk relative or absolute leafref paths.
3. A combinator that handles `deref()` paths, `current()` re-rooting, and the
   `parent`/`child`/`predicate` step kinds.
4. A way to express "this type is a leafref to *that* target" in the public AST
   so backends can share the resolved target type.

### Concrete failures observed without it

- `voice`, `openconfig-if-ip`, and `openconfig-network-instance` produce
  `{list, undefined}` for leafref-leaf-lists where the reference compiler
  produces `{list, {ns, t_target_type}}` together with a derivation that
  carries the target's restrictions.

---

## 3. Global type registry (`lookup_type`)

### What it is

A registry indexed by `BuiltinTypeName | (ModuleName, TypedefName)` that
returns the fully-resolved `Type` for any reference, including built-ins
(`string`, `int32`, `boolean`, ...), user-defined typedefs, and union member
types.

In yanger this is `yang_types:lookup_type/2` plus `register_builtin_types`
plus `mk_type_spec`. It lives in core because compile-time validation of
defaults, `range`/`length`/`pattern` restrictions, and `if-feature`-gated type
selection all need to consult it.

### Why backends need it

Any backend that emits a typed representation of a leaf must collapse a chain
of typedefs into a single description. Without a centralised registry, each
backend reimplements this walk, and inevitably handles unions, leafrefs, and
identityrefs slightly differently.

For yangest-core itself, this is also a prerequisite for #2.

### Suggested API shape

```rust
pub struct TypeRegistry { /* indexes built-ins + module-qualified typedefs */ }

impl TypeRegistry {
    pub fn lookup_builtin(&self, name: BuiltInType) -> &TypeDescr;
    pub fn lookup(&self, module: &str, name: &str) -> Option<&TypeDescr>;
    pub fn resolve(&self, type_stmt: &TypeStmt, ctx_module: &str)
        -> Result<ResolvedType, TypeError>;
}
```

---

## 4. XPath dep-path normalisation

### What it is

Transformation of a parsed XPath expression into a normalised "dependency
path" — the set of relative keypaths whose values an XPath expression depends
on. In yanger:

- `yang_xpath:compile` — compiles an `XPathExpr` into a form that can be
  executed against a cursor.
- `yang_xpath:dep_path_to_cursor_path` — converts a dep-path into a sequence
  of cursor steps, with explicit per-step namespace context.
- `yang_xpath:set_default_namespace` — applies the YANG default-namespace rule
  (unprefixed name uses the *defining* module's namespace, not the current
  module's).
- `yang_xpath_deps:is_path_absolute` — predicates over normalised paths.

### Why backends need it

The FXS format stores `validatemfas` and `cs.deps` as cursor-relative keypaths
with explicit namespaces per step. Without normalisation:

- `[..,<unknown>]` is emitted instead of `[.]` for sibling references inside
  `case` (because the cursor abstraction collapses transparent containers but
  raw XPath does not).
- Leafref paths lose namespace context across steps that cross module
  boundaries (e.g. an `oc-if:interface/oc-eth:ethernet/oc-vlan:vlan-id` path
  must keep `oc-eth` and `oc-vlan` per step rather than collapsing to the
  current module's default).

### Concrete failures observed without it

- `Cisco-IOS-XE-acl`: `[['..', <unknown>]]` instead of `[['.']]` for a `when`
  on a `case` referencing its sibling `protocol`.
- `openconfig-if-ip`: leafref paths whose namespace context across steps does
  not match the reference.

---

## 5. Source-position tracking on every statement

### What it is

Every parsed statement should carry `{file, line, column}`, and that position
should be queryable from compile passes. yanger does this with
`yang:stmt_pos/1` and `yang_error:fmt_pos/1`.

### Why backends need it

The reference compiler (and any backend that wants to *match* it byte-for-byte)
applies augments and processes uses-expansion substatements in *source order*.
Two augments on the same target are not commutative for the purposes of CS
record ordering, hash-dictionary insertion order, or `ns_to_prefix_maps`
encounter order. Without source position, the only deterministic order
available is "module name then statement keyword", which does not match any
reasonable reference.

### Required pieces

1. Promote `(file, line, column)` from a transient parser concern to a field
   on every `Stmt` and `SchemaNode`.
2. In `core/src/compiler/expansion.rs::apply_augments`, sort augments by
   `(source_file, source_pos)` before applying.
3. Iterate substatement lists in declaration order during `uses` expansion,
   not in keyword-grouped order.

### Concrete failures observed without it

- `Cisco-IOS-XE-native`, `policy`, `voice`,
  `openconfig-network-instance`: CS record ordering and hash-dict insertion
  order diverge from the reference even though the *content* of each record is
  identical.

---

## 6. Fully-qualified-path keys for the annotation index

### What it is

The annotation index (`core/src/annindex/mod.rs`) currently keys annotations
by tag (or short path). The reference compiler keys annotations by the
fully-qualified schema path of the annotated node, including the module of
each step.

### Why backends need it

Two annotations on same-named siblings in different module contexts collide
under tag-only keying, so the wrong annotation is applied (or both get
applied, or neither). This produces wrong `cs.extra` / `cs.flags` content.

### Required pieces

1. Extend the annindex key from `Tag` to `Vec<(ModuleName, NodeName)>`.
2. Update lookups to walk the path from the annotated node to root and match
   on full module-qualified path.
3. Provide a "structurally compatible" matcher for cases where the annotation
   targets a parent of the current node (deviations on a list inside an
   augment, etc.).

### Concrete failures observed without it

- `Cisco-IOS-XE-acl`, `Cisco-IOS-XE-otv`: annotation modules
  (`*-ann.yang`) attach the wrong annotations because two same-named siblings
  in different submodules collide on the index key.

---

## 7. Incremental namespace registration during type resolution

### What it is

The reference compiler's `ns_to_prefix_maps` field is built incrementally:
every time the compiler resolves a leafref or derived type, it calls an
internal `add_ns_to_prefix_map_id` that prepends the source module to a list.
The final list is therefore in *encounter order during type resolution*, not
in any sorted order.

### Why backends need it

The encounter order is content-dependent: it differs between modules whose
leafrefs are encountered in different sequences. Any pure post-pass sort
(by namespace, by module name, by tuple) can satisfy at most a subset of
modules. Verified empirically: `(ns, name)` sort works for all but
`openconfig-mpls`; `(name, ns)` sort works for all but `openconfig-if-aggregate`.

### Required pieces

1. yangest-core should expose a per-module "type-resolution event stream"
   (e.g. a callback or an iterator) that fires once per resolved leafref/
   typedef in encounter order, naming the source module each time.
2. The fxs (or any other) backend can subscribe and build its own ordered
   namespace map without needing to retrofit one from a sorted set.

### Suggested API shape

```rust
pub trait TypeResolutionObserver {
    fn on_leafref_resolved(&mut self, source_module: &str, target: &SchemaNode);
    fn on_typedef_resolved(&mut self, source_module: &str, typedef: &TypeDef);
}

impl Compiler<'_> {
    pub fn compile_module_with_observer(
        &mut self,
        module: &Module,
        observer: &mut dyn TypeResolutionObserver,
    ) -> Result<CompiledModule, CompileError>;
}
```

### Concrete failures observed without it

- `openconfig-if-aggregate` ↔ `openconfig-mpls`: choosing either sort key
  fixes one and breaks the other; only encounter-order matches both.

---

## Cross-cutting: what is the rough size?

| Feature | Approx. yanger size | Notes |
|---|---|---|
| 1 cursor | ~520 LoC | `yang_cursor.erl` |
| 2 leafref target resolution | ~250 LoC | inside `yang_types.erl` |
| 3 type registry | ~600 LoC | spread across `yang_types.erl` |
| 4 xpath dep-path normalisation | ~470 LoC | `yang_xpath_deps.erl` plus parts of `yang_xpath.erl` |
| 5 source-position tracking | ~50 LoC of plumbing | small AST extension + sort in `apply_augments` |
| 6 FQ-path annotation index | ~150 LoC | rework of `annindex` |
| 7 incremental ns registration | ~100 LoC | callback plumbing through compile |

In total this is roughly 2 000–2 500 lines of new yangest-core code, which is
disproportionately small compared to the multi-thousand-line plugins it would
unblock.

---

## Cross-cutting: prerequisites

A reasonable build order is:

```
            ┌── 1 cursor ──────────────┐
            │                          │
3 type registry ──┤            ├── 2 leafref target resolution
            │                          │
            └── 4 xpath dep-path ──────┘
                   │
                   └── 6 FQ-path annotation index
                   └── 7 incremental ns registration

5 source-position tracking — orthogonal, can land any time
```

Cursor (1), type registry (3), and source-position tracking (5) are the three
foundational additions; (2), (4), (6), and (7) layer on top.

---

## Summary

yangest-core today is sufficient for backends that emit a *flat* projection of
the compiled schema (e.g. simple JSON-tree dumps, listings, schema diff
reports). It is not yet sufficient for backends that need:

- to resolve leafref targets,
- to express dependency paths across module boundaries,
- to mirror a reference compiler's ordering decisions byte-for-byte, or
- to apply annotations/deviations to specific path-disambiguated nodes.

The seven additions listed above close that gap with a focused, mostly
mechanical body of work in yangest-core.
