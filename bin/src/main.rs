// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufWriter};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use clap::{CommandFactory, FromArgMatches, Parser};
use rayon::prelude::*;

use yangest_core::annindex::AnnotationIndex;
use yangest_core::astannindex::AstAnnotationIndex;
use yangest_core::ast::{self, ModuleKey};
use yangest_core::compiler::{ExpansionCtx, ModuleRegistry, compile_module};
use yangest_core::depgraph::DepGraph;
use yangest_core::devindex::DeviationIndex;
use yangest_core::plugin::{AstOverlayDescriptor, EmitState, OverlayExtension, Plugin, PluginRegistration};

mod bundle;

include!(concat!(env!("OUT_DIR"), "/plugin_externs.rs"));

/// Core CLI options (format, paths, features, inputs).
/// Plugin-specific options are declared by the plugins themselves via
/// [`Plugin::cli_args`] and injected into the command at runtime.
#[derive(Parser, Debug)]
#[command(
    name = "yangest",
    version,
    about = "Parallel YANG schema validator and converter"
)]
struct Cli {
    /// Output format (tree, depend, yang, yin, swagger)
    #[arg(short = 'f', long, default_value = "tree")]
    format: String,

    /// Load modules, search paths, features, and deviations from a bundle
    /// file.  The bundle is the authoritative source; it cannot be combined
    /// with positional FILE arguments or -p / --feature / --deviation-module /
    /// --annotation-module.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["inputs", "path", "features", "deviation_modules", "annotation_modules"])]
    bundle: Option<PathBuf>,

    /// Add a directory to the YANG module search path (may be repeated)
    #[arg(short = 'p', long, value_name = "DIR")]
    path: Vec<PathBuf>,

    /// Validate only — print errors/warnings but produce no output
    #[arg(short, long)]
    errors_only: bool,

    /// Enable a feature as MODULE:FEATURE (may be repeated)
    #[arg(long = "feature", value_name = "MODULE:FEATURE")]
    features: Vec<String>,

    /// Ignore unknown features in if-feature expressions (treat as disabled).
    #[arg(long)]
    ignore_unknown_features: bool,

    /// Filter output to nodes with status at most this level.
    /// current = show only current; deprecated = also show deprecated;
    /// obsolete = show all (default).
    #[arg(long, value_name = "current|deprecated|obsolete")]
    max_status: Option<String>,

    /// Apply deviations from FILE without emitting output for it (may be
    /// repeated).  Equivalent to yanger's --deviation-module.
    #[arg(long = "deviation-module", value_name = "FILE")]
    deviation_modules: Vec<PathBuf>,

    /// Apply plugin-declared annotations from FILE without emitting output for
    /// it (may be repeated).  Annotation modules contain extension statements
    /// (e.g. `tailf:annotate`) that target schema nodes by path.
    #[arg(long = "annotation-module", value_name = "FILE")]
    annotation_modules: Vec<PathBuf>,

    /// Write one output file per input module to DIR instead of stdout.
    /// Files are named <module-name>.<ext> where <ext> is determined by the
    /// active plugin (e.g. "json" for swagger, "tree" for tree).
    #[arg(long, value_name = "DIR", conflicts_with_all = ["output_file", "outputs"])]
    output_dir: Option<PathBuf>,

    /// Write all output to FILE instead of stdout.
    #[arg(short = 'o', long, value_name = "FILE", conflicts_with_all = ["output_dir", "outputs"])]
    output_file: Option<PathBuf>,

    /// Per-module output pattern: INPAT=>OUTPAT where % matches the file stem.
    /// Example: --outputs "%.yang=>build/%.tree" writes each module to
    /// build/<stem>.tree.  Cannot be combined with -o or --output-dir.
    #[arg(long, value_name = "INPAT=>OUTPAT", conflicts_with_all = ["output_file", "output_dir"])]
    outputs: Option<String>,

    /// Maximum number of modules to emit in parallel when writing per-module
    /// output files (--output-dir or --outputs).  Defaults to the number of
    /// logical CPUs.  Set to 1 to disable parallel emission.
    #[arg(short = 'j', long, value_name = "N")]
    jobs: Option<usize>,

    #[arg(value_name = "FILE", required_unless_present = "bundle")]
    inputs: Vec<PathBuf>,
}

fn main() {
    // Collect all plugins registered via inventory::submit! in plugin crates.
    // Sorted by name for consistent --help output and format lookup.
    let mut plugins: Vec<Box<dyn Plugin>> = inventory::iter::<PluginRegistration>
        .into_iter()
        .map(|r| (r.factory)())
        .collect();
    plugins.sort_by_key(|p| p.name());

    // Build the clap command from the derive-generated base, then extend it
    // with each plugin's own argument declarations.
    let mut cmd = Cli::command();
    for plugin in &plugins {
        for arg in plugin.cli_args() {
            cmd = cmd.arg(arg);
        }
    }

    // Parse all arguments (core + plugin) in one pass.
    let matches = cmd.get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());

    // Let each plugin configure itself from the parsed matches.
    for plugin in &mut plugins {
        plugin.configure(&matches);
    }

    // Build the grammar registry from all plugin declarations.
    // This is done once before any module is compiled so the compiler can
    // validate extension usage throughout the compilation process.
    let mut grammar_registry = yangest_core::grammar::GrammarRegistry::new();
    for plugin in &plugins {
        grammar_registry.register(plugin.yang_grammar());
    }

    let plugin = plugins
        .iter()
        .find(|p| p.name() == cli.format.as_str())
        .unwrap_or_else(|| {
            let names: Vec<_> = plugins.iter().map(|p| p.name()).collect();
            eprintln!(
                "yangest: unknown format '{}'. Available: {}",
                cli.format,
                names.join(", ")
            );
            std::process::exit(1);
        });

    // Validate --outputs pattern early so we fail before doing any real work.
    let output_pattern = match &cli.outputs {
        Some(pat) => match parse_output_pattern(pat) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("yangest: {}", e);
                std::process::exit(1);
            }
        },
        None => None,
    };

    // Resolve compilation inputs from either the bundle file or CLI flags.
    let (primary_paths, deviation_paths, annotation_paths, search_paths, bundle_features) =
        match &cli.bundle {
            Some(bundle_path) => {
                let mut bf = match bundle::BundleFile::load(bundle_path) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("yangest: {}", e);
                        std::process::exit(1);
                    }
                };
                let base = bundle_path
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .to_path_buf();
                bf.resolve_paths(&base);
                let feats = parse_bundle_features(&bf.features, &bf.global_features);
                (bf.modules, bf.deviation_modules, bf.annotation_modules, bf.search_paths, Some(feats))
            }
            None => (
                cli.inputs.clone(),
                cli.deviation_modules.clone(),
                cli.annotation_modules.clone(),
                cli.path.clone(),
                None,
            ),
        };

    let input_files = scan_files(&primary_paths, &[]);
    if input_files.is_empty() {
        eprintln!("yangest: no .yang files found");
        std::process::exit(1);
    }
    // Deviation-module files: deviations applied but no output emitted
    let deviation_files = scan_files(&deviation_paths, &[]);
    // Annotation-module files: annotations applied but no output emitted
    let annotation_files = scan_files(&annotation_paths, &[]);
    // Search path files: compiled as dependencies but not emitted
    let search_path_files = scan_files(&[], &search_paths);

    // Tag each file: 2 = display input, 1 = overlay (deviation/annotation), 0 = search-path dep
    let tagged: Vec<(PathBuf, u8)> = input_files
        .iter()
        .map(|p| (p.clone(), 2u8))
        .chain(deviation_files.iter().map(|p| (p.clone(), 1u8)))
        .chain(annotation_files.iter().map(|p| (p.clone(), 1u8)))
        .chain(search_path_files.iter().map(|p| (p.clone(), 0u8)))
        .collect();

    let parsed: Vec<(ModuleKey, ast::Stmt, Vec<ast::YError>, PathBuf, u8)> = tagged
        .par_iter()
        .filter_map(|(path, tag)| {
            let src = std::fs::read_to_string(path).ok()?;
            let file_arc: Arc<str> = Arc::from(path.to_string_lossy().as_ref());
            let (stmts, errs) = yangest_core::parser::parse_yang(&src, Arc::clone(&file_arc));
            stmts.into_iter().next().map(|stmt| {
                let name = stmt.arg.clone().unwrap_or_default();
                let revision = stmt
                    .get_substmt(ast::BuiltInKeyword::Revision)
                    .and_then(|r| r.arg.clone());
                let key = ModuleKey::new(name, revision);
                (key, stmt, errs, path.clone(), *tag)
            })
        })
        .collect();

    let mut all_parse_errors: Vec<ast::YError> = Vec::new();
    let mut file_order: Vec<ModuleKey> = Vec::with_capacity(parsed.len());
    let mut input_keys: HashSet<ModuleKey> = HashSet::new();
    let mut path_map: HashMap<ModuleKey, PathBuf> = HashMap::with_capacity(parsed.len());
    let mut modules: Vec<(ModuleKey, ast::Stmt)> = parsed
        .into_iter()
        .map(|(k, s, mut errs, path, tag)| {
            all_parse_errors.append(&mut errs);
            if tag == 2 {
                // display input: emit output and apply deviations
                file_order.push(k.clone());
                input_keys.insert(k.clone());
            } else if tag == 1 {
                // deviation-only: apply deviations but suppress output
                input_keys.insert(k.clone());
            }
            path_map.insert(k.clone(), path);
            (k, s)
        })
        .collect();

    let user_names: HashSet<&str> = file_order.iter().map(|k| k.name.as_str()).collect();
    for path in system_module_files() {
        let src = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let file_arc: Arc<str> = Arc::from(path.to_string_lossy().as_ref());
        let (stmts, mut errs) = yangest_core::parser::parse_yang(&src, Arc::clone(&file_arc));
        all_parse_errors.append(&mut errs);
        if let Some(stmt) = stmts.into_iter().next() {
            let name = stmt.arg.clone().unwrap_or_default();
            if user_names.contains(name.as_str()) {
                continue;
            }
            let revision = stmt
                .get_substmt(ast::BuiltInKeyword::Revision)
                .and_then(|r| r.arg.clone());
            let key = ModuleKey::new(name, revision);
            modules.push((key, stmt));
        }
    }

    // Build a map from module name → parsed stmt for all search-path modules.
    // Then walk transitively from the input set (primary + deviation modules)
    // through import/include edges to find the reachable closure, and discard
    // any search-path module that nothing in that closure imports.
    let stmt_by_name: HashMap<&str, &ast::Stmt> =
        modules.iter().map(|(k, s)| (k.name.as_str(), s)).collect();

    let mut reachable: HashSet<String> = input_keys.iter().map(|k| k.name.clone()).collect();
    let mut queue: VecDeque<String> = reachable.iter().cloned().collect();
    while let Some(name) = queue.pop_front() {
        let Some(stmt) = stmt_by_name.get(name.as_str()) else {
            continue;
        };
        for sub in &stmt.substmts {
            let dep_name = if sub.keyword.is_builtin(ast::BuiltInKeyword::Import)
                || sub.keyword.is_builtin(ast::BuiltInKeyword::Include)
            {
                sub.arg.as_deref().unwrap_or("").to_string()
            } else {
                continue;
            };
            if reachable.insert(dep_name.clone()) {
                queue.push_back(dep_name);
            }
        }
    }
    modules.retain(|(k, _)| reachable.contains(k.name.as_str()));

    let dep_graph = DepGraph::build(&modules, &mut all_parse_errors);
    let waves = match dep_graph.topo_sort_levels() {
        Ok(w) => w,
        Err(cycle) => {
            eprintln!("yangest: circular dependency detected:");
            for k in &cycle {
                eprintln!("  {}", k);
            }
            std::process::exit(1);
        }
    };

    // Build the deviation index only from explicitly listed input files.
    // Modules that arrive via -p (search path) are available for import
    // resolution but must not have their deviations applied automatically —
    // deviation files are designed to be applied selectively.
    let input_modules: Vec<&(ModuleKey, ast::Stmt)> = modules
        .iter()
        .filter(|(k, _)| input_keys.contains(k))
        .collect();
    let dev_index = Arc::new(DeviationIndex::build_from_refs(&input_modules));

    // Collect overlay extension registrations from all plugins, then build
    // the annotation index from the same overlay-module pool.
    let overlay_exts: Vec<OverlayExtension> = plugins
        .iter()
        .flat_map(|p| p.overlay_extensions().iter().cloned())
        .collect();
    let ann_index = Arc::new(AnnotationIndex::build(
        &input_modules
            .iter()
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect::<Vec<_>>(),
        &overlay_exts,
    ));

    // Collect AST-level overlay descriptors and build the AST annotation index.
    let ast_overlay_descs: Vec<AstOverlayDescriptor> = plugins
        .iter()
        .flat_map(|p| p.ast_overlay_extensions().iter().cloned())
        .collect();
    let ast_ann_index = Arc::new(AstAnnotationIndex::build(
        &input_modules
            .iter()
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect::<Vec<_>>(),
        &ast_overlay_descs,
    ));

    let mut stmt_map: HashMap<ModuleKey, ast::Stmt> = modules.into_iter().collect();
    let mut initial_registry = ModuleRegistry::new();
    initial_registry.grammar = grammar_registry;
    initial_registry.flags.ignore_unknown_features = cli.ignore_unknown_features;
    let registry = Arc::new(RwLock::new(initial_registry));
    let mut all_compile_errors: Vec<ast::YError> = Vec::new();

    for wave in waves {
        let wave_work: Vec<(ModuleKey, ast::Stmt)> = wave
            .into_iter()
            .filter_map(|key| stmt_map.remove(&key).map(|stmt| (key, stmt)))
            .collect();

        let reg_read = registry.read().unwrap();
        let compiled_wave: Vec<yangest_core::compiler::CompiledModule> = wave_work
            .into_par_iter()
            .map(|(key, stmt)| {
                let stmt = ast_ann_index.apply(stmt, &key.name);
                let mut compiled = compile_module(&key, stmt, &reg_read, &dev_index, &ann_index);
                compiled.source_path = path_map.get(&key).cloned();
                compiled
            })
            .collect();
        drop(reg_read);

        let mut reg_write = registry.write().unwrap();
        for compiled in compiled_wave {
            all_compile_errors.extend(compiled.errors.iter().cloned());
            reg_write.insert(Arc::new(compiled));
        }
    }

    let mut all_errors = all_parse_errors;
    all_errors.extend(all_compile_errors);

    let has_errors = all_errors.iter().any(|e| e.level == ast::Level::Error);
    for e in &all_errors {
        eprintln!("{}", e);
    }
    if cli.errors_only {
        std::process::exit(if has_errors { 1 } else { 0 });
    }

    let reg = registry.read().unwrap();
    let (enabled_features, global_features) = if let Some(feats) = bundle_features {
        feats
    } else {
        match parse_enabled_features(&cli.features) {
            Ok(features) => features,
            Err(err) => {
                eprintln!("yangest: {}", err);
                std::process::exit(1);
            }
        }
    };
    let max_status = match &cli.max_status {
        None => None,
        Some(s) => match parse_max_status(s) {
            Ok(status) => Some(status),
            Err(e) => {
                eprintln!("yangest: {}", e);
                std::process::exit(1);
            }
        },
    };

    // Build a fresh ExpansionCtx for one emission task.
    // Each parallel worker gets its own instance because ExpansionCtx holds a
    // per-call RefCell cache that is !Sync.  Construction is cheap: the ctx
    // only borrows the shared read-only registry and feature sets.
    let make_ctx = || {
        let mut ctx = ExpansionCtx::new(&reg, &enabled_features);
        if let Some(ms) = max_status {
            ctx = ctx.with_max_status(ms);
        }
        if !global_features.is_empty() {
            ctx = ctx.with_global_features(&global_features);
        }
        ctx
    };

    let display_modules: Vec<Arc<yangest_core::compiler::CompiledModule>> = file_order
        .iter()
        .rev()
        .filter_map(|k| reg.modules.get(k).cloned())
        .collect();

    // Build a rayon thread pool sized to --jobs (default: logical CPUs).
    let n_jobs = cli.jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(n_jobs)
        .build()
        .unwrap_or_else(|e| {
            eprintln!("yangest: failed to build thread pool: {e}");
            std::process::exit(1);
        });

    // Call prepare_bundle once before any emission (sequential, no parallelism
    // constraints).  The result is Arc'd so it can be shared across parallel workers.
    let bundle_ctx = make_ctx();
    let bundle = Arc::new(plugin.prepare_bundle(&display_modules, &reg, &bundle_ctx));

    if let Some((in_pat, out_pat)) = output_pattern {
        // Pre-create all output directories before going parallel to avoid
        // races on create_dir_all for modules sharing a parent directory.
        for module in &display_modules {
            let basename = module
                .source_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("{}.yang", module.key.name));
            let out_path = apply_output_pattern(&basename, &in_pat, &out_pat)
                .unwrap_or_else(|| PathBuf::from(format!("{}.{}", module.key.name, plugin.extension())));
            if let Some(parent) = out_path.parent() {
                if !parent.as_os_str().is_empty() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        eprintln!("yangest: cannot create directory '{}': {}", parent.display(), e);
                        std::process::exit(1);
                    }
                }
            }
        }
        pool.install(|| {
            display_modules.par_iter().for_each(|module| {
                let basename = module
                    .source_path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("{}.yang", module.key.name));
                let out_path = apply_output_pattern(&basename, &in_pat, &out_pat)
                    .unwrap_or_else(|| PathBuf::from(format!("{}.{}", module.key.name, plugin.extension())));
                let file = match std::fs::File::create(&out_path) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("yangest: cannot write '{}': {}", out_path.display(), e);
                        return;
                    }
                };
                let mut writer = BufWriter::new(file);
                let ctx = make_ctx();
                if let Err(e) = plugin.emit_module(module, &reg, &ctx, &bundle, &mut EmitState::new(), &mut writer) {
                    eprintln!("yangest: emit error for '{}': {}", module.key.name, e);
                }
            });
        });
    } else if let Some(ref outfile) = cli.output_file {
        let file = match std::fs::File::create(outfile) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("yangest: cannot create '{}': {}", outfile.display(), e);
                std::process::exit(1);
            }
        };
        let mut writer = BufWriter::new(file);
        if let Err(e) = plugin.emit(&display_modules, &reg, &make_ctx(), &mut writer) {
            eprintln!("yangest: emit error: {}", e);
        }
    } else if let Some(ref dir) = cli.output_dir {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!(
                "yangest: cannot create output directory '{}': {}",
                dir.display(),
                e
            );
            std::process::exit(1);
        }
        let ext = plugin.extension();
        pool.install(|| {
            display_modules.par_iter().for_each(|module| {
                let filename = format!("{}.{}", module.key.name, ext);
                let file = match std::fs::File::create(dir.join(&filename)) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("yangest: cannot write '{}': {}", filename, e);
                        return;
                    }
                };
                let mut writer = BufWriter::new(file);
                let ctx = make_ctx();
                if let Err(e) = plugin.emit_module(module, &reg, &ctx, &bundle, &mut EmitState::new(), &mut writer) {
                    eprintln!("yangest: emit error for '{}': {}", module.key.name, e);
                }
            });
        });
    } else {
        let stdout = io::stdout();
        let mut out = BufWriter::new(stdout.lock());
        if let Err(e) = plugin.emit(&display_modules, &reg, &make_ctx(), &mut out) {
            eprintln!("yangest: emit error: {}", e);
        }
    }
    std::process::exit(if has_errors { 1 } else { 0 });
}

fn scan_files(inputs: &[PathBuf], extra_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for input in inputs.iter().chain(extra_paths.iter()) {
        collect_yang_files(input, &mut files);
    }
    files
}

fn collect_yang_files(path: &PathBuf, out: &mut Vec<PathBuf>) {
    if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("yang") {
        out.push(path.clone());
    } else if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("yang") {
                    out.push(p);
                }
            }
        }
    }
}

fn parse_max_status(s: &str) -> Result<yangest_core::compiler::Status, String> {
    match s {
        "current" => Ok(yangest_core::compiler::Status::Current),
        "deprecated" => Ok(yangest_core::compiler::Status::Deprecated),
        "obsolete" => Ok(yangest_core::compiler::Status::Obsolete),
        _ => Err(format!(
            "invalid --max-status '{}': expected current, deprecated, or obsolete",
            s
        )),
    }
}

fn parse_enabled_features(
    raw: &[String],
) -> Result<(HashSet<(String, String)>, HashSet<String>), String> {
    let mut qualified = HashSet::new();
    let mut global = HashSet::new();
    for entry in raw {
        if let Some((module, features)) = entry.split_once(':') {
            if module.is_empty() {
                return Err(format!(
                    "invalid --feature '{}': module name cannot be empty \
                     (use 'FEATURE' without a colon for a global feature)",
                    entry
                ));
            }
            for feat in features.split(',') {
                let feat = feat.trim();
                if feat.is_empty() {
                    return Err(format!(
                        "invalid --feature '{}': feature name cannot be empty",
                        entry
                    ));
                }
                qualified.insert((module.to_string(), feat.to_string()));
            }
        } else {
            // Bare name — enable this feature in every module that declares it.
            let name = entry.trim();
            if name.is_empty() {
                return Err("invalid --feature '': feature name cannot be empty".to_string());
            }
            global.insert(name.to_string());
        }
    }
    Ok((qualified, global))
}

fn parse_bundle_features(
    map: &std::collections::HashMap<String, Vec<String>>,
    global: &[String],
) -> (HashSet<(String, String)>, HashSet<String>) {
    let mut qualified = HashSet::new();
    for (module, features) in map {
        for feat in features {
            qualified.insert((module.clone(), feat.clone()));
        }
    }
    let global_set: HashSet<String> = global.iter().cloned().collect();
    (qualified, global_set)
}

/// Parse an `--outputs` pattern of the form `INPAT=>OUTPAT`.
/// Both sides must contain exactly one `%` (the stem wildcard).
fn parse_output_pattern(pattern: &str) -> Result<(String, String), String> {
    let Some((in_pat, out_pat)) = pattern.split_once("=>") else {
        return Err(format!(
            "invalid --outputs '{}': expected INPAT=>OUTPAT",
            pattern
        ));
    };
    let in_pat = in_pat.trim().to_string();
    let out_pat = out_pat.trim().to_string();
    if !in_pat.contains('%') {
        return Err(format!(
            "invalid --outputs '{}': input pattern must contain '%'",
            pattern
        ));
    }
    if !out_pat.contains('%') {
        return Err(format!(
            "invalid --outputs '{}': output pattern must contain '%'",
            pattern
        ));
    }
    Ok((in_pat, out_pat))
}

/// Apply an output pattern to a source file's basename.
/// Returns `None` if the basename does not match the input pattern.
///
/// Example: `basename="foo.yang"`, `in_pat="%.yang"`, `out_pat="build/%.tree"`
/// → `Some(PathBuf::from("build/foo.tree"))`
fn apply_output_pattern(basename: &str, in_pat: &str, out_pat: &str) -> Option<PathBuf> {
    let pct = in_pat.find('%')?;
    let before = &in_pat[..pct];
    let after = &in_pat[pct + 1..];
    let stem = basename.strip_prefix(before)?.strip_suffix(after)?;

    let out_pct = out_pat.find('%')?;
    let out_before = &out_pat[..out_pct];
    let out_after = &out_pat[out_pct + 1..];
    Some(PathBuf::from(format!("{}{}{}", out_before, stem, out_after)))
}

fn system_module_files() -> Vec<PathBuf> {
    if let Ok(dir) = std::env::var("YANGEST_MODULES_DIR") {
        let mut files = Vec::new();
        collect_yang_files(&PathBuf::from(dir), &mut files);
        return files;
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.as_path();
        for _ in 0..5 {
            if let Some(parent) = dir.parent() {
                dir = parent;
                let candidate = dir.join("modules");
                if candidate.is_dir() {
                    let mut files = Vec::new();
                    collect_yang_files(&candidate, &mut files);
                    return files;
                }
            }
        }
    }
    Vec::new()
}
