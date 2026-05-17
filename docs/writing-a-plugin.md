# Writing a Yangest Plugin

Yangest produces output by running a *plugin* against the compiled YANG schema. Every
output format — `tree`, `yang`, `yin`, `depend`, `swagger` — is a plugin. Plugins are
ordinary Rust crates that implement a single trait; they carry no knowledge of how
files are parsed, how dependencies are resolved, or how modules are parallelised.
This document explains the API and then walks through the complete `tree` plugin as
a worked example.

---

## Table of Contents

1. [The compilation pipeline](#1-the-compilation-pipeline)
2. [The `Plugin` trait](#2-the-plugin-trait)
3. [Core types available to plugins](#3-core-types-available-to-plugins)
4. [Creating a plugin crate](#4-creating-a-plugin-crate)
5. [Registering a plugin in the binary](#5-registering-a-plugin-in-the-binary)
6. [Output modes](#6-single-file-and-batch-output-modes)
7. [The tree plugin — a complete walkthrough](#7-the-tree-plugin--a-complete-walkthrough)

---

## 1. The compilation pipeline

Before a plugin is called, yangest has already done all the heavy work:

1. **Parse** — every `.yang` file is parsed into an AST (`ast::Stmt`).
2. **Dependency graph** — import and include relationships are resolved and a
   topological sort determines compilation order.
3. **Deviation index** — deviation statements from explicitly listed input files are
   collected and indexed by their target module name.
4. **Compile** — each module is compiled in topological order. Compilation expands
   groupings, resolves typedefs, collects augments, and applies deviations. The result
   is an `Arc<CompiledModule>` stored in the `ModuleRegistry`.
5. **Emit** — the selected plugin's `emit` or `emit_module` method is called with the
   set of *display modules* (the modules the user listed as positional FILE arguments)
   and the fully-populated registry.

A plugin only participates in step 5. Everything before that point is handled by
yangest's core and is invisible to the plugin.

---

## 2. The `Plugin` trait

```rust
// core/src/plugin.rs

pub trait Plugin: Send + Sync {
    fn name(&self) -> &'static str;

    fn extension(&self) -> &'static str { self.name() }

    fn cli_args(&self) -> Vec<Arg> { vec![] }

    fn configure(&mut self, _matches: &ArgMatches) {}

    fn overlay_extensions(&self) -> &'static [OverlayExtension] { &[] }

    fn ast_overlay_extensions(&self) -> &'static [AstOverlayDescriptor] { &[] }

    fn yang_grammar(&self) -> &'static [ExtensionGrammar] { &[] }

    fn prepare_bundle(
        &self,
        _modules: &[Arc<CompiledModule>],
        _registry: &ModuleRegistry,
        _ctx: &ExpansionCtx<'_>,
    ) -> BundleState {
        BundleState::new()
    }

    fn emit(
        &self,
        modules: &[Arc<CompiledModule>],
        registry: &ModuleRegistry,
        ctx: &ExpansionCtx<'_>,
        out: &mut dyn Write,
    ) -> std::io::Result<()> {
        let bundle = self.prepare_bundle(modules, registry, ctx);
        let mut first = true;
        for module in modules {
            if !first { writeln!(out)?; }
            first = false;
            self.emit_module(module, registry, ctx, &bundle, &mut EmitState::new(), out)?;
        }
        Ok(())
    }

    fn emit_module(
        &self,
        _module: &Arc<CompiledModule>,
        _registry: &ModuleRegistry,
        _ctx: &ExpansionCtx<'_>,
        _bundle: &BundleState,
        _state: &mut EmitState,
        _out: &mut dyn Write,
    ) -> std::io::Result<()> {
        Ok(())
    }
}
```

### `name`

Returns the format name used with the `-f` flag.  Must be unique across all
registered plugins.  Examples: `"tree"`, `"swagger"`, `"yang"`.

### `extension`

Returns the file extension (without a leading dot) used when yangest writes one file
per module in `--output-dir` mode.  The default implementation returns `self.name()`,
which is correct for formats whose extension matches the format name (`tree` → `.tree`,
`yang` → `.yang`, `yin` → `.yin`).  Override when the canonical extension differs — the
swagger plugin returns `"json"` so files are named `module.json` rather than
`module.swagger`.

### `cli_args`

Returns any `clap::Arg` values that this plugin needs on the command line. Argument IDs
and long names should be namespaced with the plugin name (e.g. `"tree-depth"` /
`--tree-depth`) to avoid collisions with core options and other plugins. Return an empty
`Vec` (the default) for plugins with no extra options.

### `configure`

Called once, after CLI parsing, with the global `ArgMatches`. Extract the plugin's own
arguments here and apply them to `self`. Plugins without CLI arguments can leave the
default no-op in place.

### `overlay_extensions`

Returns a static slice of `OverlayExtension` entries that describe YANG extension
statements in overlay modules that this plugin wants to treat as per-node annotations.

When yangest processes `--annotation-module FILE` (or `annotation_modules` in a bundle),
any occurrence of a registered extension in those files whose argument is an absolute
schema-node path will have its body sub-statements injected into the target node's
`extensions` list at expansion time.

```rust
#[derive(Debug, Clone)]
pub struct OverlayExtension {
    pub module: &'static str,       // YANG module defining the extension
    pub name: &'static str,         // local extension name
    pub source_plugin: &'static str, // value of Plugin::name() for tagging
}
```

Declare entries as a module-level static and return them from `overlay_extensions()`:

```rust
static ANN_EXTS: &[OverlayExtension] = &[
    OverlayExtension {
        module: "tailf-common",
        name: "annotate",
        source_plugin: "my-plugin",
    },
];
fn overlay_extensions(&self) -> &'static [OverlayExtension] { ANN_EXTS }
```

Plugins that do not process annotation overlays can leave the default `&[]` in place.

### `ast_overlay_extensions`

Returns a static slice of `AstOverlayDescriptor` entries. These describe YANG extension
statements in overlay modules that target **source-level AST statements** — groupings,
typedefs, and the module statement itself — by selector string rather than by schema-node
path.

This is the mechanism used by `tailf:annotate-module` / `tailf:annotate-statement`. The
overlay is applied *before* compilation, directly modifying the `ast::Stmt` tree. It is
intended for statements that have no schema-node identity at compile time.

```rust
#[derive(Debug, Clone)]
pub struct ExtensionId {
    pub module: &'static str,   // YANG module defining the extension
    pub name:   &'static str,   // local extension name
}

#[derive(Debug, Clone)]
pub struct AstOverlayDescriptor {
    /// Extension that selects the *target module* by name.
    /// Its argument value must be the unqualified YANG module name.
    /// Example: `tailf:annotate-module "ietf-interfaces"`
    pub module_selector_ext: ExtensionId,

    /// Extension that selects a statement *within* that module by selector string.
    /// Selector syntax: `"keyword"` or `"keyword[name='value']"`.
    /// Example: `tailf:annotate-statement "grouping[name='interface']"`
    pub stmt_selector_ext: ExtensionId,
}
```

Declare entries as a module-level static and return them from `ast_overlay_extensions()`:

```rust
static AST_ANN_DESCS: &[AstOverlayDescriptor] = &[
    AstOverlayDescriptor {
        module_selector_ext: ExtensionId { module: "tailf-common", name: "annotate-module" },
        stmt_selector_ext:   ExtensionId { module: "tailf-common", name: "annotate-statement" },
    },
];
fn ast_overlay_extensions(&self) -> &'static [AstOverlayDescriptor] { AST_ANN_DESCS }
```

Yangest builds an `AstAnnotationIndex` from all loaded modules (including annotation and
deviation modules) before compilation begins. For each module, `apply()` is called with the
owned AST statement, injecting matching sub-statements from every overlay that targets it.

Plugins that do not need AST-level overlays can leave the default `&[]` in place.

Returns a static slice of `ExtensionGrammar` entries that describe YANG extension
statements understood by this plugin. Rules are collected from all registered plugins
before compilation begins. The compiler uses the merged grammar registry to validate
extension usage and collect `ExtensionInstance` values on schema nodes. See the
`ExtensionGrammar` section below for the full type description.

### `prepare_bundle`

Called exactly once, before any per-module emission (and before parallel workers
are spawned). Returns a [`BundleState`](#bundlestate) that is wrapped in an `Arc`
and passed as the `bundle` argument to every `emit_module` call.

Use this hook to pre-compute data that is derived from the full registry or the
complete module list — for example, a namespace cache built by iterating all
modules — when computing it once here avoids repeated work inside `emit_module`.

The default implementation returns an empty `BundleState`; plugins with nothing
global to pre-compute do not need to override this.

### `emit`

Called once with the full ordered list of display modules and an `ExpansionCtx`. The
default implementation calls `prepare_bundle` once, then iterates the list and calls
`emit_module` for each module, separated by a blank line, with a fresh `EmitState`
per module. This is the correct behaviour for all per-module formats.

Override `emit` directly only for *all-at-once* formats that need to see all modules
simultaneously before producing any output. The swagger plugin does this because a
Swagger 2.0 document is a single JSON object that aggregates paths from all listed
modules, not a concatenation of per-module documents.

### `emit_module`

Called once per display module by the default `emit` implementation.
Implement this method for per-module formats (tree, yang, yin, depend).

Both `module`/`registry` and `ctx` are passed. The registry contains **all** compiled
modules — both display modules and their search-path dependencies — so a plugin can look
up any module that was reachable from the display set. The `ctx` object carries the
expansion configuration (enabled features, max-status, deviation overlays) and is
needed whenever the plugin traverses the compiled tree via `module.children(ctx)` or
`node.children(ctx)` (see `ExpansionCtx` below).

`bundle` is the read-only `BundleState` produced by `prepare_bundle`. Use
`bundle.get::<T>()` to retrieve pre-computed data.

`state` is a fresh `EmitState` created for this module call. Use
`state.get_or_insert::<T>()` to keep mutable per-module scratch state without
`&mut self` or interior mutability on the plugin struct.

---

## 3. Core types available to plugins

All types live in `yangest_core::compiler` (re-exported via `yangest-core`).

### `CompiledModule`

The fully compiled representation of one YANG module.

| Field | Type | Description |
|-------|------|-------------|
| `key` | `ModuleKey` | Name and optional revision string. |
| `yang_version` | `YangVersion` | `V1` or `V1_1`. |
| `namespace` | `String` | YANG namespace URI. |
| `prefix` | `String` | The module's own prefix declaration. |
| `prefix_map` | `PrefixMap` (`IndexMap<String, String>`) | Maps imported prefixes to module names. |
| `children` | `Vec<SchemaNode>` | Raw top-level data-tree nodes. **Do not iterate this directly.** Use `module.children(ctx)` to get the expanded, deviation-applied, and if-feature-filtered view. |
| `augments` | `Vec<AugmentEntry>` | Augment statements this module contributes to other modules. |
| `overlay` | `NodeOverlayMap` | Deviation and annotation overlays keyed by node path. Used internally by `children(ctx)`. |
| `groupings` | `Arc<IndexMap<String, Grouping>>` | Named groupings available for `uses`. |
| `typedefs` | `IndexMap<String, Typedef>` | Named typedefs. |
| `features` | `IndexMap<String, Feature>` | Feature declarations. |
| `identities` | `IndexMap<String, Identity>` | Identity declarations. |
| `imports` | `Vec<String>` | Import module names in declaration order. |
| `includes` | `Vec<String>` | Submodule names referenced by `include`. |
| `source_path` | `Option<PathBuf>` | Filesystem path the module was loaded from. |
| `stmt` | `Stmt` | The raw AST root statement (useful for plugins that need access to uncompiled data). |
| `errors` | `Vec<YError>` | Compilation errors for this module. |
| `extensions` | `Vec<ExtensionInstance>` | Extension statements declared at module level. |
| `pmap` | `PMap` | Type-keyed extension map for plugin-private per-module state (see below). |
| `set_pdata<T>` | method | Store a `Send + Sync` value of type `T` into the module's pdata slot. |
| `pdata<T>` | method | Retrieve a reference to the previously stored value of type `T`, or `None`. |

### `SchemaNode`

A single node in the compiled data tree (container, leaf, list, choice, RPC, …).

| Field | Type | Description |
|-------|------|-------------|
| `name` | `String` | Local node name. |
| `module_name` | `String` | Name of the module that **defines** this node (may differ from the module being rendered when the node was augmented in). |
| `module_prefix` | `String` | Own prefix of the defining module, used as a fallback when rendering augmented nodes. |
| `pos` | `Pos` | Source file location (file path and line number). |
| `status` | `Status` | `Current`, `Deprecated`, or `Obsolete`. |
| `config` | `Option<bool>` | Explicit `config` statement; `None` means "inherit from parent". |
| `when` | `Vec<WhenExpr>` | `when` XPath constraints on this node (empty if none). |
| `if_features` | `Vec<IfFeatureExpr>` | `if-feature` guards; evaluated by `ExpansionCtx` to determine node visibility. |
| `description` | `Option<String>` | Description string from the YANG source, if present. |
| `reference` | `Option<String>` | Reference string from the YANG source, if present. |
| `extensions` | `Vec<ExtensionInstance>` | Resolved extension instances on this node (see `ExtensionInstance` below). |
| `kind` | `SchemaNodeKind` | Variant-typed payload carrying kind-specific data (see below). |
| `pmap` | `PMap` | Plugin-private per-node state (not cloned when nodes are copied into augment groups). |

**Accessing children:** do not iterate `kind.children` directly. Call
`node.children(ctx)` instead. This method expands any `Uses` nodes in the raw
`kind.children` list by inlining the referenced grouping body, applies refinements and
deviation overlays, filters out `if-feature`-disabled nodes, and returns a plain
`Vec<SchemaNode>`. The result reflects exactly what a consuming device would see.

```rust
// Correct — lazy expansion, deviations and if-features applied:
let children = node.children(ctx);

// Wrong — raw pre-expansion list, may contain Uses nodes:
// match &node.kind { SchemaNodeKind::Container { children, .. } => children, … }
```

### `SchemaNodeKind`

An enum with one variant per YANG data-node type:

```rust
pub enum SchemaNodeKind {
    Container { presence: Option<String>, children: Vec<SchemaNode>, musts: Vec<MustExpr> },
    Leaf      { type_stmt: Stmt, units: Option<String>, default: Option<String>,
                mandatory: bool, musts: Vec<MustExpr> },
    LeafList  { type_stmt: Stmt, units: Option<String>, default: Vec<String>,
                min_elements: u64, max_elements: Option<u64>,
                ordered_by: OrderedBy, musts: Vec<MustExpr> },
    List      { key: Vec<String>, unique: Vec<String>, children: Vec<SchemaNode>,
                min_elements: u64, max_elements: Option<u64>,
                ordered_by: OrderedBy, musts: Vec<MustExpr> },
    Choice    { default: Option<String>, mandatory: bool, cases: Vec<SchemaNode> },
    Case      { children: Vec<SchemaNode> },
    Rpc       { input: Vec<SchemaNode>, output: Vec<SchemaNode>, musts: Vec<MustExpr> },
    Action    { input: Vec<SchemaNode>, output: Vec<SchemaNode> },
    Notification { children: Vec<SchemaNode>, musts: Vec<MustExpr> },
    AnyXml    { mandatory: bool, musts: Vec<MustExpr> },
    AnyData   { mandatory: bool, musts: Vec<MustExpr> },
    // Plugins never match on this variant directly — call node.children(ctx) instead.
    Uses      { grouping: Arc<Grouping>, source_module_name: Option<String>, overlay: UsesOverlay },
}
```

`Leaf` and `LeafList` carry the raw `type_stmt` AST node rather than a resolved type
object.  The full type (including typedef resolution) is preserved in the AST; plugins
that need the YANG type name can read `type_stmt.arg`.

The `Uses` variant represents a lazy grouping instantiation. Plugins should never
match on it directly; `node.children(ctx)` expands it transparently, returning the
inlined grouping body with refinements and deviation overlays applied.

### `AugmentEntry`

Records one `augment` statement contributed by a module to some other module's tree.

| Field | Type | Description |
|-------|------|-------------|
| `target_path` | `Vec<PathStep>` | Absolute schema path to the augment target. Each `PathStep` has an optional `prefix` and a `name`. |
| `nodes` | `Vec<SchemaNode>` | The nodes injected at that path. |
| `when` | `Vec<WhenExpr>` | `when` constraints on the augment. |
| `if_features` | `Vec<IfFeatureExpr>` | `if-feature` guards on the augment. |

### `ModuleRegistry`

A read-only view of all compiled modules, keyed by `ModuleKey`.

```rust
pub struct ModuleRegistry {
    pub modules: IndexMap<ModuleKey, Arc<CompiledModule>>,
    pub grammar: GrammarRegistry,
    pub flags:   CompilationFlags,
}
```

`grammar` holds the merged extension grammar rules from all plugins (built before
compilation begins). `flags` carries runtime flags such as `ignore_unknown_features`.

Useful registry methods:

```rust
registry.get(&key)                     // exact lookup
registry.resolve_import(name, rev)     // latest-revision fallback lookup
```

`resolve_import` first tries an exact `name@revision` match, then tries the synthetic
`latest` key, and finally does a linear scan — it always returns the best available
match for a given name.

### `ExpansionCtx`

The expansion context carries the configuration that governs how the compiled tree is
traversed. It is constructed by the binary and passed to both `emit` and `emit_module`.

```rust
pub struct ExpansionCtx<'_> {
    pub enabled_features: HashSet<(String, String)>,  // (module_name, feature_name)
    pub max_status: Option<Status>,
    // ... (internal overlay state)
}
```

The key methods plugins use:

```rust
// Expand the top-level children of a module (deviations, if-features, groupings):
let top: Vec<SchemaNode> = module.children(ctx);

// Expand the children of a non-leaf node:
let children: Vec<SchemaNode> = node.children(ctx);
```

Both `module.children(ctx)` and `node.children(ctx)` perform lazy grouping expansion,
apply use-site refinements, filter out `if-feature`-disabled nodes, and return the final
visible node list. Nodes with a `status` worse than `ctx.max_status` (if set) are also
filtered out.

Do **not** access `module.children` or `kind.children` directly; those are the
pre-expansion raw lists.

### `ExtensionGrammar`

Describes the grammar of a YANG extension statement understood by a plugin:

```rust
pub struct ExtensionGrammar {
    pub module:   &'static str,      // e.g. "tailf-common"
    pub name:     &'static str,      // e.g. "callpoint"
    pub parents:  Vec<GrammarParent>, // valid parent statement kinds; empty = any
    pub arg_type: Option<ArgType>,   // None = no argument
    pub substmts: Vec<SubstmtSpec>,  // allowed sub-statements
}
```

Declare grammar rules as a module-level static and return them from `yang_grammar()`:

```rust
use yangest_core::grammar::{ExtensionGrammar, ArgType};

static MY_GRAMMAR: &[ExtensionGrammar] = &[
    ExtensionGrammar {
        module:   "acme-ext",
        name:     "purpose",
        parents:  vec![],                    // allowed anywhere
        arg_type: Some(ArgType::String),
        substmts: vec![],
    },
];

impl Plugin for MyPlugin {
    fn yang_grammar(&self) -> &'static [ExtensionGrammar] { MY_GRAMMAR }
    // ...
}
```

### `ExtensionInstance`

A resolved extension instance collected from a schema node or module:

```rust
pub struct ExtensionInstance {
    pub module:   String,           // resolved module name
    pub name:     String,           // local extension name
    pub arg:      Option<String>,   // argument value, if any
    pub substmts: Vec<Stmt>,        // raw sub-statements for plugin access
}
```

Access extension instances via:
- `node.extensions` — instances on a `SchemaNode`
- `module.extensions` — instances at module level (direct `module`-body sub-statements)

For targeted lookup by extension name:

```rust
let instances: Vec<&ExtensionInstance> = node.extensions.iter()
    .filter(|e| e.module == "acme-ext" && e.name == "purpose")
    .collect();
```

### The `pmap` extension map

Both `CompiledModule` and `SchemaNode` carry a `PMap` (`HashMap<TypeId, Box<dyn Any + Send + Sync>>`).
This lets a plugin attach private data to nodes without changing the core types.
Access by inserting or reading with a plugin-specific newtype as the key:

```rust
struct MyAnnotation(String);
node.pmap.insert(TypeId::of::<MyAnnotation>(), Box::new(MyAnnotation("hello".into())));
let ann = node.pmap.get(&TypeId::of::<MyAnnotation>());
```

Note that `pmap` is deliberately **not cloned** when `SchemaNode` is cloned (e.g. when
augment groups are assembled). Private state therefore does not carry over into
secondary copies of a node.

### `BundleState`

Bundle-level shared state produced once by `prepare_bundle` and passed read-only to
every `emit_module` call. Values stored here must be `Send + Sync` because the map
may be shared across parallel workers.

```rust
// In prepare_bundle:
let mut bundle = BundleState::new();
let data: &mut MyBundleData = bundle.get_or_insert::<MyBundleData>();
data.ns_cache.insert("ietf-inet-types".into(), "urn:ietf:params:xml:ns:yang:ietf-inet-types".into());
bundle

// In emit_module:
if let Some(data) = bundle.get::<MyBundleData>() {
    // read data.ns_cache …
}
```

| Method | Description |
|--------|-------------|
| `get_or_insert::<T>() -> &mut T` | Insert `T::default()` if absent; return a mutable reference. Call during `prepare_bundle`. |
| `get::<T>() -> Option<&T>` | Return a shared reference to a previously inserted value. Call during `emit_module`. |

### `EmitState`

Per-module mutable scratch storage, created fresh for every `emit_module` call.
Values need only be `Send` (not `Sync`).

```rust
#[derive(Default)]
struct MyState { counter: u32 }

fn emit_module(&self, module, registry, ctx, bundle, state: &mut EmitState, out) {
    let s: &mut MyState = state.get_or_insert::<MyState>();
    s.counter += 1;
    // …
}
```

| Method | Description |
|--------|-------------|
| `get_or_insert::<T>() -> &mut T` | Insert `T::default()` if absent; return a mutable reference. |

### `AppliedDeviations` and `AppliedAnnotations`

The compiler stores which deviation modules and annotation modules were applied to
each primary module in the module's pdata slot. Retrieve them with `pdata`:

```rust
use yangest_core::compiler::{AppliedDeviations, AppliedAnnotations};

// Each entry is (deviating_module_name, Option<revision>)
if let Some(devs) = module.pdata::<AppliedDeviations>() {
    for (name, rev) in &devs.0 {
        eprintln!("  deviated by: {name}");
    }
}
if let Some(anns) = module.pdata::<AppliedAnnotations>() {
    for (name, rev, _prefix_map) in &anns.0 {
        eprintln!("  annotated by: {name}");
    }
}
```

### `find_child_in_raw`

```rust
pub fn find_child_in_raw(
    target_name: &str,
    raw: &[SchemaNode],
    overlay: &NodeOverlayMap,
    ctx: &ExpansionCtx<'_>,
) -> Option<SchemaNode>
```

An early-termination variant of child expansion: stops as soon as the first visible
child with `name == target_name` is found, lazily expanding `uses` groupings only as
needed. Useful for navigating augment target paths step-by-step in large modules
without the O(n) cost of a full `children(ctx)` expansion.

---

## 4. Creating a plugin crate

Plugins live under `plugins/<name>/`.  The minimal crate structure:

```
plugins/myplugin/
  Cargo.toml
  src/
    lib.rs
```

**`plugins/myplugin/Cargo.toml`**:

```toml
[package]
name    = "yangest-plugin-myplugin"
version = "0.1.0"
edition = "2024"

[dependencies]
yangest-core = { path = "../../core" }
inventory    = "0.3"
```

**`plugins/myplugin/src/lib.rs`**:

```rust
use std::io::Write;
use std::sync::Arc;

use yangest_core::compiler::{CompiledModule, ExpansionCtx, ModuleRegistry};
use yangest_core::plugin::{BundleState, EmitState, Plugin, PluginRegistration};

pub struct MyPlugin;

impl Plugin for MyPlugin {
    fn name(&self) -> &'static str { "myplugin" }

    fn emit_module(
        &self,
        module: &Arc<CompiledModule>,
        _registry: &ModuleRegistry,
        ctx: &ExpansionCtx<'_>,
        _bundle: &BundleState,
        _state: &mut EmitState,
        out: &mut dyn Write,
    ) -> std::io::Result<()> {
        writeln!(out, "# {}", module.key.name)?;
        for child in module.children(ctx) {
            writeln!(out, "  {}", child.name)?;
        }
        Ok(())
    }
}

inventory::submit! {
    PluginRegistration { factory: || Box::new(MyPlugin) }
}
```

Add the new crate to the workspace root `Cargo.toml`:

```toml
[workspace]
members = [
    "core",
    "bin",
    "plugins/tree",
    "plugins/myplugin",   # add this
    ...
]
```

---

## 5. Registering a plugin in the binary

Yangest uses the [`inventory`](https://docs.rs/inventory) crate for build-time
self-registration. Each plugin crate submits itself via an `inventory::submit!`
call (shown above). The binary picks up all registered plugins automatically at
startup — no manual list in `main.rs` required.

To wire a new plugin into the binary you need only two things:

**1. Declare the dependency in `bin/Cargo.toml`:**

```toml
[dependencies]
yangest-plugin-myplugin = { path = "../plugins/myplugin" }
```

**2. Add the crate to the workspace root `Cargo.toml`:**

```toml
[workspace]
members = [
    "core",
    "bin",
    "plugins/tree",
    "plugins/myplugin",   # add this
    ...
]
```

After `cargo build`, the new format is available as `-f myplugin`. Nothing in
`main.rs` needs to change.

#### How self-registration works

`inventory::submit!` uses linker-section tricks (a `#[used]` static placed in a
dedicated object-file section) so that each `PluginRegistration` value is
collected into a global list before `main` runs. Because Rust's linker only pulls
in object files that are actually referenced, `bin/build.rs` generates a small
`plugin_externs.rs` file with `extern crate yangest_plugin_xxx;` declarations for
every `yangest-plugin-*` dependency found in `bin/Cargo.toml`. These declarations
force the linker to include each plugin crate's object files, ensuring the
`inventory::submit!` statics are present at link time.

---

## 6. Single-file and batch output modes

### Single file: `-o`

When the user passes `-o <FILE>`, all output is redirected to that file instead of
stdout. This is handled entirely in the binary; plugins require no changes.

### Batch output: `--output-dir`

When the user passes `--output-dir DIR`, the binary calls `plugin.emit` once per
display module, writing each result to `DIR/<module-name>.<extension>`. The extension
comes from `plugin.extension()`.

This is handled entirely in the binary — plugins require no changes to support batch
mode. The `emit` default implementation (one call to `emit_module` per module)
automatically does the right thing. An all-at-once plugin that overrides `emit` will
also work correctly: in batch mode it is called once per module with a single-element
slice, so it naturally produces per-module output.

### Pattern-based output: `--outputs`

`--outputs INPAT=>OUTPAT` is a generalised per-module routing mode. The `%`
character is a stem wildcard that matches the source file's basename (without
extension, up to but not including the first `.` after the prefix).

```
# For each *.yang input file, write a *.tree output alongside it
yangest --outputs "%.yang=>%.tree" *.yang

# Write into a build/ subdirectory
yangest --outputs "%.yang=>build/%.tree" *.yang
```

The binary creates intermediate directories as needed. Plugins require no changes
to support `--outputs` — it uses the same per-module `emit` call as `--output-dir`.

`--outputs` cannot be combined with `-o` or `--output-dir`.

---

## 7. The tree plugin — a complete walkthrough

The tree plugin (`plugins/tree/src/lib.rs`) emits RFC 8340 YANG tree diagrams.  It is
the primary verification format used to check that yangest's output matches yanger's.
This chapter follows the source top-to-bottom, explaining every function in detail.

### 7.1 Overview and plugin entry point

```rust
#[derive(Default)]
pub struct TreePlugin {
    depth: Option<usize>,
}

impl Plugin for TreePlugin {
    fn name(&self) -> &'static str { "tree" }

    fn cli_args(&self) -> Vec<Arg> {
        vec![
            Arg::new("tree-depth")
                .long("tree-depth")
                .value_name("N")
                .value_parser(clap::value_parser!(usize))
                .help("Limit tree output depth"),
        ]
    }

    fn configure(&mut self, matches: &ArgMatches) {
        self.depth = matches.get_one::<usize>("tree-depth").copied();
    }

    fn emit_module(
        &self,
        module: &Arc<CompiledModule>,
        registry: &ModuleRegistry,
        ctx: &ExpansionCtx<'_>,
        out: &mut dyn Write,
    ) -> std::io::Result<()> {
        let all_top = module.children(ctx);
        if all_top.is_empty() && module.augments.is_empty() {
            return Ok(());
        }
        writeln!(out, "module: {}", module.key.name)?;

        if !all_top.is_empty() {
            let augments_in = collect_incoming_augments(module, registry);
            emit_children(
                &all_top, &[], &augments_in,
                module, "  ", None, &[], 0, ctx, out,
            )?;
        }

        if !module.augments.is_empty() {
            writeln!(out)?;
            for aug in &module.augments {
                emit_augment_section(aug, module, ctx, self.depth, out)?;
            }
        }

        Ok(())
    }
}
```

`TreePlugin` derives `Default` and carries a `depth: Option<usize>` field that limits
tree output to N levels of nesting when `--tree-depth N` is given. The plugin declares
the `--tree-depth` argument via `cli_args()` and reads it back in `configure()`.

The entry logic has three steps:

1. **Expand top-level children.** `module.children(ctx)` returns the fully expanded,
   deviation-applied, if-feature-filtered list of top-level nodes. If that list and the
   augment list are both empty, the module produces no tree output.

2. **Own data tree.** `collect_incoming_augments` gathers nodes that other modules
   augment *into* this module's tree. Then `emit_children` renders the top-level nodes
   with an initial indent of two spaces.

3. **Outgoing augment sections.** Each `AugmentEntry` is rendered as a separate
   `augment /path:` section below the main tree.

### 7.2 Outgoing augment sections

```rust
fn emit_augment_section(
    aug: &AugmentEntry,
    module: &CompiledModule,
    ctx: &ExpansionCtx<'_>,
    depth: Option<usize>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let path_str = format_augment_path(&aug.target_path);
    writeln!(out, "  augment {path_str}:")?;
    let empty_map = HashMap::new();
    emit_children(
        &aug.nodes, &[], &empty_map,
        module, "    ", None, &[], 0, ctx, out,
    )
}
```

Each outgoing augment is introduced by a header line `  augment /prefix:target/sub:`
followed by its contributed nodes indented four spaces deep. The augmented nodes can
themselves contain containers, lists, and so on, so `emit_children` is used for
recursive rendering just as it is for the main tree. An empty `AugmentMap` is passed
because the augment section itself will not receive further incoming cross-module
augments at render time.

```rust
fn format_augment_path(path: &[PathStep]) -> String {
    let mut s = String::new();
    for step in path {
        s.push('/');
        if let Some(pfx) = &step.prefix {
            let _ = write!(s, "{pfx}:");
        }
        s.push_str(&step.name);
    }
    s
}
```

`format_augment_path` converts a `Vec<PathStep>` into the canonical
`/prefix:name/name/name` form. The first step in an augment path always carries the
target module's prefix (e.g. `/oc-if:interfaces`); subsequent steps within the same
module are typically unqualified (they inherit the previous step's namespace context).

### 7.3 Collecting incoming augments

The most algorithmically complex part of the tree plugin is assembling the nodes that
foreign modules contribute to the module being rendered. RFC 8340 requires these nodes
to appear *in-line* at the correct position in the tree — not in separate sections — so
the tree plugin must merge them with the module's own children.

```rust
type AugmentMap = HashMap<Vec<String>, Vec<Vec<SchemaNode>>>;

fn collect_incoming_augments(
    target: &CompiledModule,
    registry: &ModuleRegistry,
) -> AugmentMap {
    let mut result: AugmentMap = HashMap::new();
    for other in registry.modules.values() {
        let mut target_augments: Vec<(Vec<String>, Vec<SchemaNode>)> = Vec::new();
        for aug in &other.augments {
            if let Some(first) = aug.target_path.first() {
                let resolves_to = first
                    .prefix.as_deref()
                    .and_then(|p| other.prefix_map.get(p))
                    .map(|s| s.as_str())
                    .unwrap_or(other.key.name.as_str());
                if resolves_to == target.key.name {
                    let path: Vec<String> = aug.target_path.iter()
                        .map(|step| step.name.clone()).collect();
                    target_augments.push((path, aug.nodes.clone()));
                }
            }
        }
        if target_augments.is_empty() { continue; }
        // ... ordering logic (see below)
    }
    result
}
```

The function iterates every module in the registry. For each module `other`, it scans
`other.augments` for entries whose target path resolves to `target`. Resolution uses the
prefix of the first path step: look up that prefix in `other.prefix_map` to get the
target module name. If the first step has no prefix, the augment targets the augmenting
module itself (a module augmenting its own tree, which is unusual but allowed).

The result type `AugmentMap` maps a path (as a `Vec<String>` of local names, dropping
prefixes) to a list of *groups* of nodes. The outer `Vec` represents the ordering of
augment statements from different source positions; the inner `Vec<SchemaNode>` is one
group of nodes contributed by one `augment` statement. When rendering, all groups
targeting a path are appended to the module's own children at that path, in source order.

#### Augment ordering: the first-claim algorithm

Yanger preserves the order in which augmented nodes appear at a given location in the
tree based on the source order of `augment` statements. When module A augments path
`P/X` and a later statement augments path `P` directly adding node `Y`, the position
of `Y` in the rendered tree at `P` is determined by the first time any `augment`
statement mentioned something at `P` — which was the `P/X` augment.

The tree plugin replicates this with a *first-claim* map:

```rust
let mut first_claim: HashMap<Vec<String>, HashMap<String, usize>> = HashMap::new();
for (i, (path, nodes)) in target_augments.iter().enumerate() {
    // Direct: each node being added to `path`
    for node in nodes {
        first_claim.entry(path.clone()).or_default()
            .entry(node.name.clone()).or_insert(i);
    }
    // Indirect: each proper prefix claims the next path element
    for j in 1..path.len() {
        let parent = path[0..j].to_vec();
        let child_name = path[j].clone();
        first_claim.entry(parent).or_default()
            .entry(child_name).or_insert(i);
    }
}
```

`first_claim[parent_path][child_name]` holds the index `i` of the first augment
statement (from `target_augments`, in source order) that mentioned `child_name` at
`parent_path`, either directly (by adding `child_name` to `parent_path`) or indirectly
(by augmenting a deeper path that passes through `parent_path/child_name`).

Augment groups are then sorted by their *effective position* — the minimum first-claim
index across all their contributed node names:

```rust
let mut path_groups: HashMap<Vec<String>, Vec<(usize, usize, Vec<SchemaNode>)>> = HashMap::new();
for (i, (path, nodes)) in target_augments.into_iter().enumerate() {
    let eff_pos = nodes.iter()
        .filter_map(|n| first_claim.get(&path).and_then(|m| m.get(&n.name)).copied())
        .min().unwrap_or(i);
    path_groups.entry(path).or_default().push((eff_pos, i, nodes));
}

for (path, mut groups) in path_groups {
    groups.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    for (_, _, nodes) in groups {
        result.entry(path.clone()).or_default().push(nodes);
    }
}
```

Using source index `i` as a tiebreaker ensures that two groups with identical effective
positions preserve their original relative order.

### 7.4 Display names for augmented nodes

```rust
fn node_display_name(node: &SchemaNode, module: &CompiledModule) -> String {
    if node.module_name == module.key.name {
        node.name.clone()
    } else {
        let prefix = module.prefix_map
            .iter()
            .find(|(_, v)| v.as_str() == node.module_name.as_str())
            .map(|(pfx, _)| pfx.as_str())
            .unwrap_or(node.module_prefix.as_str());
        format!("{}:{}", prefix, node.name)
    }
}
```

RFC 8340 §3.1 requires that nodes augmented from a foreign module be shown as
`prefix:name` rather than bare `name`. The prefix to use is looked up in the *target
module's* `prefix_map` (the module being rendered): if the target module imports the
augmenting module, it has a prefix alias for it and that alias is used. If the target
module does not import the augmenting module (indirect dependency via a third module),
the augmenting module's own self-declared prefix is used as a fallback, stored in
`node.module_prefix`.

### 7.5 Width computation for column alignment

```rust
fn compute_width(nodes: &[SchemaNode], module: &CompiledModule, ctx: &ExpansionCtx<'_>) -> usize {
    nodes.iter().map(|n| match &n.kind {
        SchemaNodeKind::Choice { .. } => 3 + compute_width(&n.children(ctx), module, ctx),
        SchemaNodeKind::Case { .. }   => 3 + compute_width(&n.children(ctx), module, ctx),
        _ => node_display_name(n, module).len(),
    }).max().unwrap_or(0)
}
```

RFC 8340 tree diagrams align the type column across siblings so that all type
annotations in a sibling group start at the same column. `compute_width` computes the
maximum display-name width in a sibling group. For `choice` and `case` nodes, the
display form adds three characters of markup (`(`, `:`, `)` punctuation or similar), so
the effective width budget is 3 less for their children; `compute_width` accounts for
this by adding 3 back when recursing into cases.

```rust
fn mk_name(name_with_suffix: &str, max_name_width: usize) -> String {
    let min_w = max_name_width + 1;
    let len = name_with_suffix.len();
    if len < min_w {
        format!("{}{}   ", name_with_suffix, " ".repeat(min_w - len))
    } else {
        format!("{}   ", name_with_suffix)
    }
}
```

`mk_name` pads a name (including any trailing suffix like `?` or `*`) to at least
`max_name_width + 1` characters, then appends three spaces before the type annotation.
Names that are already at or beyond the minimum width still get three spaces — so the
type column is never closer than three characters to the name, matching yanger's
`mk_str/2` Erlang function.

### 7.6 Recursive tree rendering

#### `emit_children`

```rust
fn emit_children(
    nodes: &[SchemaNode],
    parent_path: &[String],
    augments_in: &AugmentMap,
    module: &CompiledModule,
    prefix: &str,
    config_ctx: Option<bool>,
    list_keys: &[String],
    inherited_width: usize,
    ctx: &ExpansionCtx<'_>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let n = nodes.len();
    let width = if inherited_width > 0 { inherited_width }
                else { compute_width(nodes, module, ctx) };
    for (i, node) in nodes.iter().enumerate() {
        let is_last = i == n - 1;
        let child_prefix = if is_last {
            format!("{}   ", prefix)
        } else {
            format!("{}|  ", prefix)
        };
        emit_node(node, parent_path, augments_in, module,
                  prefix, &child_prefix, config_ctx, list_keys, width, ctx, out)?;
    }
    Ok(())
}
```

`emit_children` is the loop driver. Its key responsibilities:

- **Width.** Computes `width` for this sibling group once, so all siblings align their
  type columns. If `inherited_width > 0`, the parent (a `choice` or `case`) has already
  computed the width and is passing it down; otherwise `compute_width` is called fresh.

- **Continuation lines.** RFC 8340 draws sibling connections with `|  ` for non-last
  siblings and `   ` (three spaces) for the last sibling. `child_prefix` is the
  *continuation prefix* — the string prepended to all lines belonging to a node's
  subtree. The *line prefix* (the string prepended to the node's own line) is simply
  `prefix`, the same string used for the sibling group.

- **Parameters threaded through.** `parent_path` grows as we descend (to look up
  augments), `augments_in` is the full incoming-augment map (constant), `config_ctx`
  carries an inherited `config false` from an ancestor, and `list_keys` names the keys
  of the immediately enclosing list (so key leaves are rendered as mandatory). `ctx` is
  passed through to every recursive call.

#### `emit_node`

```rust
fn emit_node(
    node: &SchemaNode,
    parent_path: &[String],
    augments_in: &AugmentMap,
    module: &CompiledModule,
    prefix: &str,
    child_prefix: &str,
    config_ctx: Option<bool>,
    list_keys: &[String],
    width: usize,
    ctx: &ExpansionCtx<'_>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let eff_config = if config_ctx == Some(false) { Some(false) } else { node.config };

    let status = match node.status {
        Status::Deprecated => 'x',
        Status::Obsolete   => 'o',
        Status::Current    => '+',
    };

    let rw = rw_from_config(eff_config);
    let dname = node_display_name(node, module);

    let node_path: Vec<String> = {
        let mut p = parent_path.to_vec();
        p.push(node.name.clone());
        p
    };
    // ...
}
```

**Effective config.** RFC 8340 §2.2 says `config false` is inherited: once any ancestor
is non-configurable, all descendants are non-configurable too, regardless of their own
`config` statement. `eff_config` implements this: if `config_ctx` is `Some(false)`, the
effective config is forced to `false` independently of `node.config`.

**Status characters.** RFC 8340 uses the leading character of the branch marker to
encode YANG status: `+` for current, `x` for deprecated, `o` for obsolete.

**r/w marker.**

```rust
fn rw_from_config(config: Option<bool>) -> &'static str {
    match config {
        Some(false) => "ro",
        _ => "rw",
    }
}
```

`config false` (or inherited config false) means read-only (`ro`); everything else
(explicit `config true` or no `config` statement, which defaults to the parent's value)
is read-write (`rw`).

**Node path.** `node_path` is `parent_path` extended with this node's name. It is used
to look up which incoming augments target *this node's children* in `augments_in`.

#### Inline augment merging

A local helper closure `with_augments` merges incoming augmented nodes with a node's
own children when needed:

```rust
fn with_augments(
    children: &[SchemaNode],
    node_path: &[String],
    augments_in: &AugmentMap,
) -> Option<Vec<SchemaNode>> {
    augments_in.get(node_path).map(|groups| {
        let mut combined: Vec<SchemaNode> = children.to_vec();
        for group in groups {
            combined.extend(group.iter().cloned());
        }
        combined
    })
}
```

If `augments_in` has an entry for `node_path`, the augmented node groups are appended
to a copy of the node's own children. The `Option` return lets the caller fall back to
the original slice cheaply (avoiding allocation) when no augments exist.

#### Per-kind rendering

`emit_node` has a `match &node.kind` arm for each `SchemaNodeKind` variant:

**Container:**
```rust
SchemaNodeKind::Container { children, presence, .. } => {
    let p_marker = if presence.is_some() { "!" } else { "" };
    writeln!(out, "{}{}--{} {}{}", prefix, status, rw, dname, p_marker)?;
    let combined = with_augments(children, &node_path, augments_in);
    let all_children = combined.as_deref().unwrap_or(children);
    emit_children(all_children, &node_path, augments_in, module,
                  child_prefix, eff_config, &[], 0, ctx, out)?;
}
```

A presence container has `!` appended to its name (RFC 8340 §3.1). Its children
recursively follow on the next lines with `child_prefix` as their continuation prefix.
Key leaves do not apply inside containers (cleared to `&[]`).

**Leaf:**
```rust
SchemaNodeKind::Leaf { type_stmt, mandatory, .. } => {
    let is_key = list_keys.iter().any(|k| k == &node.name);
    let suffix = if *mandatory || is_key { "" } else { "?" };
    let type_str = leaf_type_display(type_stmt, &module.prefix);
    let name_col = mk_name(&format!("{}{}", dname, suffix), width);
    write!(out, "{}{}--{} {}{}", prefix, status, rw, name_col, type_str)?;
    writeln!(out)?;
}
```

A leaf is optional (trailing `?`) unless it is `mandatory true` or it is a list key
(keys are implicitly mandatory). The type column is produced by `leaf_type_display` and
placed at the alignment position computed by `mk_name`.

**Leaf-list:**
```rust
SchemaNodeKind::LeafList { type_stmt, .. } => {
    let suffix = "*";
    let type_str = leaf_type_display(type_stmt, &module.prefix);
    let name_col = mk_name(&format!("{}{}", dname, suffix), width);
    write!(out, "{}{}--{} {}{}", prefix, status, rw, name_col, type_str)?;
    writeln!(out)?;
}
```

Leaf-lists are always shown with a `*` suffix denoting zero-or-more cardinality.

**List:**
```rust
SchemaNodeKind::List { children, key, .. } => {
    let key_str = format!(" [{}]", key.join(" "));
    writeln!(out, "{}{}--{} {}*{}", prefix, status, rw, dname, key_str)?;
    let combined = with_augments(children, &node_path, augments_in);
    let all_children = combined.as_deref().unwrap_or(children);
    emit_children(all_children, &node_path, augments_in, module,
                  child_prefix, eff_config, key, 0, ctx, out)?;
}
```

Lists are shown with `*` (zero-or-more) and the key names in brackets: `* [key1 key2]`.
Key names are passed as `list_keys` to `emit_children` so that key leaf nodes inside
the list are rendered without `?`.

**Choice and case:**
```rust
SchemaNodeKind::Choice { cases, mandatory, .. } => {
    let opt = if *mandatory { "" } else { "?" };
    writeln!(out, "{}{}--{} ({}){}",  prefix, status, rw, dname, opt)?;
    let child_w = width.saturating_sub(3);
    emit_children(cases, &node_path, augments_in, module,
                  child_prefix, eff_config, &[], child_w, ctx, out)?;
}
SchemaNodeKind::Case { children } => {
    writeln!(out, "{}+--:({})", prefix, node.name)?;
    let child_w = width.saturating_sub(3);
    emit_children(children, &node_path, augments_in, module,
                  child_prefix, eff_config, list_keys, child_w, ctx, out)?;
}
```

RFC 8340 renders a `choice` as `(name)` and each `case` as `:(name)`. The three extra
characters of markup are accounted for by subtracting 3 from the width when descending
into cases and children. For `case`, `list_keys` is threaded through unchanged so that
key leaves inside a case are still rendered correctly.

**RPC and action:**
```rust
SchemaNodeKind::Rpc { input, output, .. } | SchemaNodeKind::Action { input, output } => {
    writeln!(out, "{}+---x {}", prefix, dname)?;
    let has_out = !output.is_empty();
    if !input.is_empty() {
        let inp_cont = if has_out {
            format!("{}|  ", child_prefix)
        } else {
            format!("{}   ", child_prefix)
        };
        writeln!(out, "{}+--ro input", child_prefix)?;
        emit_children(input, &node_path, augments_in, module,
                      &inp_cont, None, &[], 0, ctx, out)?;
    }
    if !output.is_empty() {
        let out_cont = format!("{}   ", child_prefix);
        writeln!(out, "{}+--ro output", child_prefix)?;
        emit_children(output, &node_path, augments_in, module,
                      &out_cont, Some(false), &[], 0, ctx, out)?;
    }
}
```

RPCs and actions are rendered with `x` in the branch marker. Both `input` and `output`
are pseudo-containers rendered as `+--ro input` / `+--ro output`. The continuation
prefix for `input` must account for whether `output` follows: if it does, a `|` must
continue down; if input is the only section, three spaces are used instead. Output is
always `config false` (it carries state only), so `Some(false)` is passed as
`config_ctx` for the output section.

**Notification:**
```rust
SchemaNodeKind::Notification { children, .. } => {
    writeln!(out, "{}+---n {}", prefix, dname)?;
    emit_children(children, &node_path, augments_in, module,
                  child_prefix, eff_config, &[], 0, ctx, out)?;
}
```

Notifications use `n` in the branch marker.

**AnyXml and anydata:**
```rust
SchemaNodeKind::AnyXml { mandatory, .. } | SchemaNodeKind::AnyData { mandatory, .. } => {
    let suffix = if *mandatory { "" } else { "?" };
    let kw = match &node.kind {
        SchemaNodeKind::AnyXml { .. } => "anyxml",
        _ => "anydata",
    };
    let name_col = mk_name(&format!("{}{}", dname, suffix), width);
    write!(out, "{}{}--{} {}   <{}>", prefix, status, rw, name_col, kw)?;
    writeln!(out)?;
}
```

Both `anyxml` and `anydata` nodes lack a meaningful type, so their type column shows the
keyword in angle brackets: `<anyxml>` or `<anydata>`.

### 7.7 Leaf type display and leafref normalisation

```rust
fn leaf_type_display(type_stmt: &yangest_core::ast::Stmt, module_prefix: &str) -> String {
    use yangest_core::ast::BuiltInKeyword;
    if type_stmt.arg.as_deref() == Some("leafref") {
        let path = type_stmt.substmts.iter()
            .find(|s| matches!(s.keyword,
                yangest_core::ast::Keyword::BuiltIn(BuiltInKeyword::Path)))
            .and_then(|s| s.arg.as_deref())
            .unwrap_or("?");
        format!("-> {}", normalize_leafref_path(path, module_prefix))
    } else {
        type_stmt.arg.as_deref().unwrap_or("unknown").to_owned()
    }
}
```

Most leaf types are displayed simply as their YANG type name (e.g. `string`,
`uint32`, `inet:ip-address`). The special case is `leafref`: RFC 8340 renders a leafref
as `-> path` rather than the bare keyword. The path is read from the `path` sub-statement
of the `type leafref` AST node.

Because leafref paths can be verbose, yanger normalises them by stripping redundant
module prefixes from consecutive path steps that stay in the same namespace. The tree
plugin replicates this with `normalize_leafref_path`:

```rust
fn normalize_leafref_path(path: &str, module_prefix: &str) -> String {
    let starts_with_slash = path.starts_with('/');
    let mut cur_prefix = module_prefix.to_owned();

    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut result: Vec<String> = Vec::with_capacity(parts.len());

    for part in parts {
        let tokens: Vec<&str> = part.split(':').filter(|s| !s.is_empty()).collect();
        match tokens.as_slice() {
            [p, name] if *p == cur_prefix.as_str() => {
                result.push((*name).to_owned());
            }
            [new_prefix, _name] => {
                result.push(part.to_owned());
                cur_prefix = (*new_prefix).to_owned();
            }
            _ => {
                result.push(tokens.join(""));
            }
        }
    }

    let joined = result.join("/");
    if starts_with_slash { format!("/{}", joined) } else { joined }
}
```

The algorithm mirrors yanger's `leafref_ptr/2` in `yanger_tree.erl`:

- Split the path on `/`, discarding empty segments (Erlang's `string:tokens` behaviour).
- Track a *current prefix*, initialised to the module's own prefix.
- For each segment, split on `:`:
  - **Two tokens, same prefix as current** — the prefix is redundant; output just the
    local name and keep `cur_prefix` unchanged.
  - **Two tokens, different prefix** — output the full `prefix:name` and update
    `cur_prefix` to the new prefix, since subsequent steps may now be in that namespace.
  - **Any other token count** (unqualified steps, or predicate segments like `[ios:key='value']`
    which split into more than two pieces) — concatenate all tokens with no separator,
    matching Erlang's iolist flattening behaviour, and leave `cur_prefix` unchanged.
- Reassemble with `/` and restore the leading `/` if the original path had one.

This produces compact, readable leafref displays like
`-> /ios:native/interface/GigabitEthernet/name` instead of the fully-qualified
`-> /ios:native/ios:interface/ios:GigabitEthernet/ios:name`.
