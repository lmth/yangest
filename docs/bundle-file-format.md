# Bundle File Format

A **bundle file** (`.yangbundle`) is a TOML file that captures the complete,
authoritative input specification for a yangest compilation job:

- which modules are the primary compilation targets,
- where to find their dependencies,
- which deviation modules to apply,
- which annotation modules to apply, and
- which YANG features are enabled.

Bundle files contain only *input* concerns. Output format, output destination, and
all other runtime options remain on the command line.

---

## Table of Contents

1. [Motivation](#1-motivation)
2. [Invoking yangest with a bundle](#2-invoking-yangest-with-a-bundle)
3. [File format reference](#3-file-format-reference)
4. [Path resolution](#4-path-resolution)
5. [Features](#5-features)
6. [Full example](#6-full-example)
7. [Writing bundle files](#7-writing-bundle-files)

---

## 1. Motivation

For small projects, passing module paths directly on the command line is
convenient:

```
yangest -p yang/ mymodule.yang
```

For larger projects — hundreds of modules, device-specific deviation files,
specific feature sets — the command line grows unwieldy and the exact set of
inputs becomes difficult to reproduce reliably. A bundle file solves this:

```
yangest --bundle project.yangbundle -f tree
```

The bundle is the single source of truth for what is compiled. It can be checked
into version control alongside the YANG source, ensuring that every developer and
CI pipeline compiles exactly the same set of inputs.

---

## 2. Invoking yangest with a bundle

```
yangest --bundle <FILE> [output options]
```

`--bundle` is mutually exclusive with the positional `FILE` arguments and with
the flags `-p`, `--feature`, `--deviation-module`, and `--annotation-module`.
Everything about *what* to compile comes from the bundle; everything about *how*
to present output stays on the command line.

Examples:

```
# Print a tree diagram for all modules in the bundle
yangest --bundle project.yangbundle -f tree

# Write one .yang file per module into build/
yangest --bundle project.yangbundle -f yang-expanded --output-dir build/

# Write output files via a stem-wildcard pattern
yangest --bundle project.yangbundle -f tree --outputs "%.yang=>reports/%.tree"

# Validate only
yangest --bundle project.yangbundle --errors-only
```

---

## 3. File format reference

Bundle files are valid TOML.  All keys are optional except `modules`.

> **Files or directories.** Every entry in the four path-list keys — `modules`,
> `search_paths`, `deviation_modules`, and `annotation_modules` — may name either
> a single `.yang` file **or a directory**. A directory entry contributes every
> `.yang` file located *directly inside* it; the scan is **not** recursive, so
> nested subdirectories are not descended (list each subdirectory explicitly, or
> the parent of a flat layout). The same applies to the equivalent command-line
> arguments (positional `FILE`, `-p`, `--deviation-module`, `--annotation-module`).
> Note that a directory only sets *where* files are found and *what role* they play
> (by which key/flag lists it) — yangest does not infer a file's role from its
> contents, so primary modules, deviation modules, and annotation modules must be
> kept in separate directories (or listed separately) to be classified correctly.

### `modules` (required)

```toml
modules = [
    "yang/ietf-interfaces.yang",
    "yang/ietf-ip.yang",
]
```

An ordered list of paths to the primary `.yang` files.  These are the modules
for which output is produced.  Each path may be absolute or relative to the
bundle file's directory (see [Path resolution](#4-path-resolution)).

### `search_paths`

```toml
search_paths = [
    "yang/",
    "/usr/share/yang/modules/ietf",
]
```

Directories that yangest scans for dependency modules.  A module found here is
compiled only if it is imported (directly or transitively) by one of the primary
modules; the rest are silently ignored.

Default: `[]` (no extra search paths).

### `deviation_modules`

```toml
deviation_modules = [
    "deviations/device-deviations.yang",
    "deviations/vendor-deviations.yang",
]
```

Deviation files that are applied to their target modules but are not themselves
included in the output.  Equivalent to passing `--deviation-module` repeatedly
on the CLI.

Default: `[]`.

### `annotation_modules`

```toml
annotation_modules = [
    "annotations/device-annotations.yang",
    "annotations/vendor-annotations.yang",
]
```

Annotation overlay files that are applied to their target nodes at expansion
time but are not themselves included in the output.  An annotation module
contains plugin-declared extension statements (e.g. an `acme:annotate`-style
extension) whose argument is an absolute schema-node path; the extension's body
sub-statements are injected into the target node's extension list when the node
is expanded.

Equivalent to passing `--annotation-module` repeatedly on the CLI.

Which extension statements are treated as annotations is determined by the
active plugins (see [`Plugin::overlay_extensions`] in the plugin API).  A YANG
file that contains no such statements is harmless — it is parsed and its imports
resolved, but it produces no annotations.

Default: `[]`.

### `[features]`

```toml
[features]
"ietf-interfaces"   = ["arbitrary-names", "pre-provisioning"]
"ietf-ip"           = ["ipv4-non-contiguous-netmasks"]
"acme-proprietary"  = []
```

A TOML table that maps module names to lists of enabled feature names.  A module
that appears with an empty list has all of its features disabled.  A module that
is not mentioned at all has all of its features enabled (the default yangest
behaviour).

Default: `{}` (all features in all modules enabled).

### `global-features`

```toml
global-features = ["file-logging", "timezone-name"]
```

A list of bare feature names that are enabled in **every** module that declares
them.  Equivalent to passing `--feature FEAT` (without a module qualifier) on
the CLI.

If a name in this list matches no feature in any loaded module, it is silently
ignored.  `global-features` may be combined with the `[features]` table; both
contribute to the set of enabled features.

Default: `[]` (no global features; enablement is determined by `[features]` alone).

---

## 4. Path resolution

All paths in a bundle file — `modules`, `search_paths`, `deviation_modules`,
and `annotation_modules` — are resolved relative to the **directory that
contains the bundle file**, not relative to the current working directory.

This makes bundles portable: the same bundle file works correctly regardless of
where you invoke yangest from, as long as the YANG files are in the expected
locations relative to the bundle itself.

Absolute paths are used as-is.  Directory entries (see the note in
[File format reference](#3-file-format-reference)) resolve the same way — a
relative directory is taken relative to the bundle file's directory, then scanned
for `.yang` files.

```toml
# If the bundle is at /project/bundles/mydevice.yangbundle, these resolve to:
#   /project/bundles/yang/ietf-interfaces.yang
#   /usr/share/yang/ietf  (absolute — used unchanged)
modules      = ["yang/ietf-interfaces.yang"]
search_paths = ["/usr/share/yang/ietf"]
```

---

## 5. Features

YANG `if-feature` guards can make large portions of a module conditional.
yangest's default behaviour — when no `[features]` table is present, no
`global-features` list is present, and no `--feature` flags are given — is to
treat all features as enabled, so the full schema is visible.

In a bundle file, the `[features]` table lets you lock down exactly which
features are active for a compilation job:

```toml
[features]
# Enable two features in ietf-interfaces, disable all others in that module
"ietf-interfaces" = ["arbitrary-names", "pre-provisioning"]

# Disable every feature in this module
"acme-device" = []
```

A module that does not appear in the `[features]` table retains the default
all-enabled behaviour.

The `global-features` list enables features by bare name across **all** modules:

```toml
global-features = ["file-logging", "timezone-name"]
```

Both keys may be present simultaneously; a feature is enabled if it appears in
either the `[features]` table (for its module) or in `global-features`.

The bundle feature keys replace the `--feature` CLI flags entirely; the two
cannot be combined.

---

## 6. Full example

```toml
# project.yangbundle
#
# Compilation bundle for the acme-router-v4 software image.
# All paths are relative to this file.

modules = [
    "yang/acme-interfaces.yang",
    "yang/acme-routing.yang",
    "yang/acme-qos.yang",
]

search_paths = [
    "yang/",
    "yang/ietf/",
    "yang/openconfig/",
]

deviation_modules = [
    "deviations/acme-router-v4-deviations.yang",
]

annotation_modules = [
    "annotations/acme-router-v4-annotations.yang",
]

[features]
"acme-interfaces" = ["roce", "breakout-capable"]
"acme-routing"    = ["mpls", "segment-routing"]
"acme-qos"        = []

global-features = ["ha-mode"]
```

With this bundle checked in at `bundles/acme-router-v4.yangbundle`, any
developer can run:

```
yangest --bundle bundles/acme-router-v4.yangbundle -f tree
```

and get the exact same compiled schema regardless of their working directory.

---

## 7. Writing bundle files

Bundle files are plain text and can be written by hand. For projects that would
rather start from an existing directory tree, the built-in `generate-bundle`
subcommand scaffolds one for you:

```
yangest generate-bundle <DIR>... [-p <DEP_DIR>]... [-o project.yangbundle]
```

It walks the tree(s) recursively and classifies each `.yang` file by inspecting
its statements:

- a plain `module` → `modules` (a primary, emitted module);
- a module with top-level `deviation` statements → `deviation_modules`;
- a module using a plugin-declared overlay/annotation extension → `annotation_modules`;
- a `submodule` → not listed; its directory is added to `search_paths` so
  `include` resolves;
- each `-p <DEP_DIR>` → carried over verbatim into `search_paths` as a
  dependency-only path.

Paths are written relative to the output file's directory (`-o`), or to the
current directory when writing to stdout.

Because some distinctions cannot be made from a file's contents alone — most
notably *primary target* versus *dependency-only* module — the generated bundle is
a **scaffold to edit**, not a final answer; everything in the scanned tree is
treated as primary, and pure dependencies are expected to come from `-p`
directories. Modules that carry *both* their own data and `deviation` statements
are classified as deviation modules but flagged with a `# note:` comment for review.

Generation is a built-in subcommand rather than an output plugin: it runs *before*
compilation, on raw parsed files (which may not yet form a compilable set), whereas
output plugins operate on the already `CompiledModule`-level schema. (A separate,
simpler capability — serialising the *resolved* inputs of a successful compile back
into a `.yangbundle` — could instead be offered as a normal post-compile facility,
since by then classification is already known.)
