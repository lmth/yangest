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
contains plugin-declared extension statements (e.g. a `tailf:annotate`-style
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

Absolute paths are used as-is.

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

Bundle files are plain text and can be written by hand. For projects that need
to generate or modify them programmatically, a future `bundle` output plugin will
be able to read the current compilation inputs and write them back out as a
`.yangbundle` file.
