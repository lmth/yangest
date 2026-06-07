# yangest

**yangest** is a parallel YANG schema compiler and multi-format converter.
It is embarrassingly similar to [yanger](https://github.com/mbj4668/yanger), in that it
supports the same output formats and extension mechanisms.

Where yangest differs is in how it handles large module collections. Rather than
processing one module at a time (which causes repeated re-parsing of shared
dependencies), yangest parses all modules once, builds a dependency graph, and
processes them in parallel waves. Additionally, yangest does not eagerly expand
`grouping`/`uses` during compilation: grouping bodies are stored by reference and
expanded lazily when a plugin traverses the tree; deviations and refinements are
kept as overlays and applied on demand. This means large shared groupings are parsed
once regardless of how many modules `use` them.

The expanded schema forest is structurally shared — node children are reference-counted
and cloned copy-on-write — so emitting a large collection stays cheap in both time and
memory. As a reference point, a ~1000-module Cisco IOS-XE bundle compiles and emits RFC
8340 tree output for every module in roughly **0.7 s using under 1 GB of peak memory** on
a 20-core host.

> **Note on provenance:** yangest is an independent from-scratch Rust implementation.
> No source code was derived from yanger. yanger is copyright Tail-f Systems AB,
> written by Martin Björklund, and is licensed under the Apache License 2.0.
> See the [NOTICE](NOTICE) file for details.

---

## Building

```
cargo build --release
```

The binary is `target/release/yangest`.

yangest uses [mimalloc](https://github.com/microsoft/mimalloc) as its global allocator —
heavy-bundle builds are allocation-bound, and mimalloc roughly halves wall-clock time
over the system allocator. It is an ordinary Cargo dependency built from source, so a C
compiler (e.g. `cc` or `clang`) must be available on the build host.

---

## Quick start

```
# Print an RFC 8340 tree diagram
yangest mymodule.yang

# Include a search-path directory for dependencies
yangest -p /path/to/yang-modules mymodule.yang

# Validate only (no output)
yangest -e mymodule.yang

# Write YANG output to a file
yangest -f yang -o expanded.yang mymodule.yang
```

---

## CLI reference

```
Usage: yangest [OPTIONS] [FILE]...
       yangest [OPTIONS] --bundle <FILE>

Arguments:
  [FILE]...  One or more .yang files to compile and display.
             Required unless --bundle is used.

Core options:
  -f, --format <FORMAT>          Output format [default: tree]
                                 Available: tree, yang, yang-expanded, yin, depend, swagger
  -p, --path <DIR>               Add a directory to the module search path (repeatable)
  -e, --errors-only              Validate only — print errors/warnings, no output

Input bundle:
      --bundle <FILE>            Load modules, search paths, deviation modules,
                                 annotation modules, and features from a
                                 .yangbundle TOML file.
                                 Cannot be combined with FILE, -p, --feature,
                                 --deviation-module, or --annotation-module.

Output options:
  -o, --output-file <FILE>       Write all output to FILE instead of stdout
      --output-dir <DIR>         Write one output file per input module to DIR.
                                 Files are named <module-name>.<ext>.
      --outputs <INPAT=>OUTPAT>  Write one output file per input module using a
                                 stem-wildcard pattern. % matches the file stem.
                                 Example: --outputs "%.yang=>build/%.tree"
                                 Creates intermediate directories as needed.

Feature / status options:
      --feature <FEAT>           Enable a feature (repeatable).
                                 Qualified form: MODULE:FEAT (or MODULE:F1,F2).
                                 Bare form: FEAT — enables the feature in every
                                 module that declares it.
      --max-status <LEVEL>       Hide nodes whose status exceeds LEVEL.
                                 LEVEL: current | deprecated | obsolete [default: obsolete]
      --ignore-unknown-features  Suppress errors for unknown module prefixes in
                                 if-feature expressions; treat them as disabled.

Overlay options:
      --deviation-module <FILE>  Apply deviations from FILE without emitting output
                                 for it (repeatable)
      --annotation-module <FILE> Apply plugin-declared annotations from FILE
                                 without emitting output for it (repeatable).
                                 Annotation modules carry extension statements
                                 whose body is injected into the target nodes at
                                 expansion time.

Format-specific options:
      --tree-depth <N>           Limit tree output to N levels of nesting
```

---

## Output formats

| `-f` value | Description |
|---|---|
| `tree` | RFC 8340 YANG tree diagram (default) |
| `yang` | Pretty-printed YANG source (raw AST re-serialised) |
| `yang-expanded` | YANG source with groupings expanded, deviations applied, and disabled-by-feature nodes removed |
| `yin` | YIN (XML) representation |
| `depend` | Module dependency list |
| `swagger` | Swagger/OpenAPI 2.0 JSON (experimental) |

---

## Status filtering

YANG nodes carry a `status` statement: `current` (the default), `deprecated`, or
`obsolete`.  `--max-status` hides nodes whose status is "worse" than the threshold:

```
# Show only current nodes (hide deprecated and obsolete)
yangest --max-status current mymodule.yang

# Show current and deprecated (hide obsolete)
yangest --max-status deprecated mymodule.yang
```

Status ordering: `current` < `deprecated` < `obsolete`.

---

## Features

YANG `if-feature` guards control whether a node appears in the compiled tree.
By default, all features are enabled. Use `--feature` to restrict to specific ones:

```
# Enable only feature 'advanced' in module 'mymod' (qualified form)
yangest --feature mymod:advanced mymodule.yang

# Enable multiple features for the same module
yangest --feature mymod:advanced,experimental mymodule.yang

# Enable features in multiple modules
yangest --feature modA:feat1 --feature modB:feat2 mymodule.yang

# Enable a feature in every module that declares it (bare/global form)
yangest --feature file-logging mymodule.yang

# Mix qualified and global features freely
yangest --feature mymod:advanced --feature file-logging mymodule.yang
```

The bare (global) form enables the named feature in **every** module that
declares it.  If no loaded module declares the feature it is silently ignored.
The two forms may be combined in any order.

---

## Deviation modules

To apply deviations from an external file without emitting output for it:

```
yangest --deviation-module my-device-deviations.yang mymodule.yang
```

---

## Search paths and lazy loading

When a `-p <DIR>` path is given, yangest scans it for `.yang` files and makes them
available as dependency candidates. Only modules actually imported by the display
modules are compiled — the rest are ignored.

---

## Writing plugins

See [docs/writing-a-plugin.md](docs/writing-a-plugin.md) for the complete plugin API
reference and a full walkthrough of the tree plugin.

## Bundle files

See [docs/bundle-file-format.md](docs/bundle-file-format.md) for the complete
`.yangbundle` file format reference.

Each path in a bundle (and the corresponding `-p` / `--deviation-module` /
`--annotation-module` flags) may name a directory; yangest includes the `.yang`
files directly inside it (one level, non-recursive). A file's *role* is set by
which key/flag lists it, not by its contents, so keep primary, deviation, and
annotation modules in separate directories.

### Generating a bundle from a directory tree

```
yangest generate-bundle <DIR>... [-p <DEP_DIR>]... [-o project.yangbundle]
```

`generate-bundle` walks the given directory tree(s), classifies each `.yang` file
by inspecting its statements, and writes a starter `.yangbundle` (to stdout, or to
`-o FILE`):

- every plain `module` becomes a **primary** (emitted) module;
- a module with top-level `deviation` statements goes to `deviation_modules`;
- a module using a plugin-declared annotation extension goes to `annotation_modules`;
- `submodule` files are not listed; their directories are added to `search_paths`
  so `include` resolves;
- directories passed with `-p` are carried over verbatim into `search_paths` as
  dependency-only paths.

The one distinction that cannot be made from a file's contents — *primary target*
vs. *dependency-only* — is resolved by convention: everything in the scanned tree
is treated as primary, and pure dependencies belong in `-p` directories. The
result is a scaffold to review and refine, not a final answer.

---

## Architecture

yangest processes YANG in five stages:

1. **Parse** — all `.yang` files are parsed in parallel into AST trees.
2. **Dependency graph** — imports and includes are resolved into a DAG; topological
   sort determines compilation order.
3. **Deviation index** — deviation statements are indexed by target module.
4. **Compile** — modules are compiled wave-by-wave in parallel (all modules in the same
   dependency wave are compiled simultaneously). Groupings are stored lazily and only
   expanded on demand when a plugin traverses the tree.
5. **Emit** — the selected plugin's `emit` method is called with the compiled modules.

Key difference from yanger: steps 1–4 happen for all modules at once rather than one
module at a time, so shared dependencies are parsed and compiled only once regardless
of how many modules import them.
