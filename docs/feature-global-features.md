# Feature: Global (module-unqualified) `--feature` flags

## Summary

Allow `--feature FEATURE` (no module prefix) in addition to the existing
`--feature MODULE:FEATURE` form.  A feature specified without a module
qualifier is treated as **globally enabled**: it enables the feature in every
module that defines it, regardless of which module that turns out to be.

## Motivation

YANG tools that predate `MODULE:FEATURE` qualification (confdc, pyang, older
yanger) accept bare feature names.  Real-world NED build systems pass features
like

```
--feature file-logging-archive-config
--feature timezone-name
--feature if-mib
```

without knowing (or caring) which YANG module declares them.  Today yangest
rejects these with

```
error: invalid --feature 'file-logging-archive-config': expected MODULE:FEATURE
```

This makes it impossible to reuse existing build scripts verbatim when
migrating to yangest.

A secondary use case is scripting: when a feature is enabled in exactly one
module in the corpus the user should not need to look up which module to
qualify it with.

## Proposed behaviour

| Invocation | Effect |
|---|---|
| `--feature mod:feat` | Enable `feat` in module `mod` only (existing behaviour, unchanged) |
| `--feature feat` | Enable `feat` in **every** module that declares a `feature feat` statement |

Both forms may be repeated and combined freely.

Resolution happens during compilation, not at argument parse time, so the set
of modules in scope determines which modules the global feature applies to.
If no loaded module defines a globally-specified feature it is silently ignored
(same as `--ignore-unknown-features` does for the qualified form).

## Interaction with `--ignore-unknown-features`

`--ignore-unknown-features` already suppresses errors for `if-feature`
expressions that reference features not present in any loaded module.  Global
features compose naturally: a bare `--feature X` that matches zero modules is
already harmless because no `if-feature X` expression will evaluate to `true`.
No special interaction is needed.

## Implementation sketch

1. **Argument parsing** (`bin/src/main.rs`): parse `--feature VALUE` and split
   on the first `:`.  If a `:` is present, push to a
   `Vec<(String, String)> qualified_features` list; otherwise push to a
   `Vec<String> global_features` list.

2. **Feature resolution** (wherever `ModuleRegistry` or `CompileCtx` resolves
   `if-feature` expressions): when checking whether a feature is enabled,
   first look it up in `qualified_features` for the current module; if not
   found, check whether its local name appears in `global_features`.

3. **`--bundle` file format**: add an optional `global-features` key (list of
   unqualified feature names) alongside the existing `features` key so that
   bundle files can express global features too.

## Compatibility

The change is fully backwards-compatible.  All existing `MODULE:FEATURE`
invocations continue to work unchanged.  The new bare-name form is additive.
