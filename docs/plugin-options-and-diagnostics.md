# Plugin command-line options and diagnostics

How a yangest plugin declares its own CLI flags, reads them (and the global
options), and raises warnings/errors — including gcc-style "treat warnings as
errors".

## 1. How yanger does it (and why we follow the same shape)

yanger (and the broader pyang-family of YANG tools it shares heritage with) is
organised around plugins that *register* capabilities into a central context at
start-up. Two registrations are relevant here:

- **Option specs.** A plugin registers a getopt-style option specification
  (`register_option_specs/2` in yanger). The core merges every plugin's specs
  into the one global option parser, so a plugin's flags appear in `--help` and
  are parsed in the same pass as the core options. The parsed options live on the
  shared context that is later handed to the plugin's output-format callback, so
  the plugin reads *both* its own options and the global ones from there.

- **Error codes + a diagnostic channel.** A plugin registers error codes, each
  with a *default severity* (error or warning) and a format string
  (`register_error_codes`). During processing a plugin emits a diagnostic by code
  and position (`yang_error:add_error(...)`); the core accumulates all
  diagnostics, sorts them by source position, prints them uniformly, and lets the
  user **override severity per code** and **treat warnings as errors** globally.
  The exit status is driven by whether any (post-override) error remains.

The two takeaways we adopt:

1. Plugin options are *declared by the plugin* and *merged centrally*; the plugin
   is then *given the parsed options* rather than parsing argv itself.
2. Diagnostics are *structured* (level + optional code + message), flow through a
   *central sink*, and severity is *policy* the host applies (warnings-as-errors)
   — not something each plugin hard-codes.

## 2. Declaring CLI options (already supported)

The `Plugin` trait already supports this; nothing new was needed.

```rust
fn cli_args(&self) -> Vec<clap::Arg> {
    vec![clap::Arg::new("myfmt-depth")
        .long("myfmt-depth")
        .value_parser(clap::value_parser!(u32))
        .help("Maximum depth to render")]
}

fn configure(&mut self, matches: &clap::ArgMatches) {
    if let Some(d) = matches.get_one::<u32>("myfmt-depth") {
        self.depth = *d;
    }
}
```

- The binary collects `cli_args()` from **every** registered plugin and adds them
  to the global `clap` command, so they show up in `--help` and parse alongside
  the core options.
- After parsing, the binary calls `configure(&matches)` on each plugin. The
  `ArgMatches` is the **full** set, so a plugin can read its own flags *and*
  global yangest flags (e.g. `--feature`, `--max-status`).
- **Convention:** prefix option IDs/long-names with the plugin name
  (`myfmt-depth`, `--myfmt-depth`) to avoid collisions — clap will refuse to start
  if two plugins declare the same ID.

> Note on activation: yangest registers every plugin's options globally (so they
> are always accepted), rather than only the active `-f <plugin>` one. This is
> simpler and means `--help` lists everything; the trade-off is that a flag for an
> inactive plugin is accepted and ignored rather than rejected. If strict
> "active-plugin-only" validation is ever wanted, `configure` is the place a
> plugin could warn (via the diagnostics channel below) when it sees one of its
> flags set while it is not the selected format.

The `depend` plugin (`--depend-recurse`, `--depend-ignore-module`, …) and
`bundle-imports` (`--bundle-imports-warn-unpinned`) are worked examples.

## 3. Raising warnings and errors (new)

Plugins emit structured diagnostics through a thread-safe sink reachable from the
emit context:

```rust
fn emit(&self, modules, registry, ctx, out) -> std::io::Result<()> {
    if let Some(diags) = ctx.diagnostics() {
        diags.warning_with_code(self.name(), "unpinned-import",
            format!("{m}: import of '{t}' is not revision-pinned"));
        // diags.error(self.name(), "…");  // always fatal
    }
    // …emit output…
}
```

- `ctx.diagnostics()` returns `Option<&Diagnostics>`
  (`yangest_core::diagnostics`). The host wires it into the `ExpansionCtx` before
  emission via `ExpansionCtx::with_diagnostics(&diags)`; it is `None` on the pure
  compile path. Plugins should treat `None` as "no sink — skip".
- `Diagnostics` is internally synchronised (a `Mutex`), so the **same** sink is
  shared by reference across the parallel per-module emit path (each worker builds
  its own `ExpansionCtx` but they all point at one `Diagnostics`).
- A `Diagnostic` carries `level` (`Warning`/`Error`), the raising `plugin`, an
  optional symbolic `code`, and a `message`. `error*` is always fatal; `warning*`
  is fatal only under `--werror`.

Why a sink on the context rather than a new `emit` parameter: it keeps the trait
and all existing plugins' signatures unchanged, and it mirrors the other ambient
emit state already on `ExpansionCtx` (`with_max_status`, `with_shared_expand_cache`,
…). It matches yanger's "diagnostics flow through the shared context" model.

## 4. `--werror` (treat warnings as errors)

`yangest --werror` makes warnings behave like errors, gcc-style:

- **Compile/parse warnings** are promoted to errors and **stop the run before any
  output is produced** (`is_fatal(level, werror)` in
  `yangest_core::diagnostics`).
- **Plugin warnings** raised during emit are collected and, under `--werror`,
  make the process exit non-zero after emission completes. (True mid-emit
  early-exit isn't attempted because emission is parallel; the run still fails,
  just after the in-flight output is written.)
- Without `--werror`, warnings are printed but do not affect the exit status —
  the previous behaviour is unchanged.

The diagnostic's `code` field is the hook for finer, gcc-`-Werror=<code>`-style
control: the host could promote only specific codes. yangest currently implements
the global `--werror`; per-code promotion is a small, additive extension on top of
the `code` already carried by every diagnostic.

### Exit-status summary

| Situation | no `--werror` | `--werror` |
|---|---|---|
| compile/parse error | exit 1, output still attempted | exit 1, **no output** |
| compile/parse warning | printed, exit 0 | exit 1, **no output** |
| plugin error | exit 1 | exit 1 |
| plugin warning | printed, exit 0 | exit 1 (after emit) |
| clean | exit 0 | exit 0 |
