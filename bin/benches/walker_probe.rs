// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! One-shot probe for `SchemaWalker` memory/time on a heavy bundle, processed
//! **as a bundle** — i.e. mirroring `bin/src/main.rs`: parallel wave compilation
//! and a parallel per-module walk sharing one cross-module expand cache.
//!
//! This is the validation harness for the Uses-body augment follow-up
//! (docs/core-walker-design.md §11): the obvious fix is unsafe at scale, so any
//! candidate must be measured here against a representative heavy module set
//! before landing.
//!
//! ```sh
//! YANGEST_BENCH_MODULES=/path/to/bundle \
//!     cargo bench --bench walker_probe --features bench-alloc
//! # limit the parallel walk to N modules / cap rayon threads:
//! YANGEST_BENCH_MODULES=/path/to/bundle YANGEST_PROBE_LIMIT=200 YANGEST_JOBS=8 \
//!     cargo bench --bench walker_probe --features bench-alloc
//! ```
//!
//! It compiles the whole bundle once (parallel waves), then runs
//! `SchemaWalker::walk` for every module in parallel — each worker with its own
//! `ExpansionCtx` attached to a shared expand cache, exactly as the real emit
//! path does — reporting modules walked, total observer events, wall-clock, and
//! (with `bench-alloc`) peak live allocation.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use rayon::prelude::*;

use yangest_core::annindex::AnnotationIndex;
use yangest_core::astannindex::AstAnnotationIndex;
use yangest_core::ast::{self, ModuleKey};
use yangest_core::compiler::{
    compile_module, CompiledModule, ExpansionCtx, ModuleRegistry, SchemaNode, Typedef,
};
use yangest_core::compiler::ExtensionInstance;
use yangest_core::depgraph::DepGraph;
use yangest_core::devindex::DeviationIndex;
use yangest_core::types_registry::{ConstraintKind, TypeRegistry, TypeResolutionObserver};
use yangest_core::walker::SchemaWalker;

// ── Optional peak-allocation tracker (feature `bench-alloc`) ──────────────────

#[cfg(feature = "bench-alloc")]
mod alloc_tracker {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CURRENT: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);

    pub struct Counting;

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let p = unsafe { System.alloc(layout) };
            if !p.is_null() {
                let cur = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
                PEAK.fetch_max(cur, Ordering::Relaxed);
            }
            p
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) };
            CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        }
    }

    pub fn peak_bytes() -> usize {
        PEAK.load(Ordering::Relaxed)
    }
    pub fn current_bytes() -> usize {
        CURRENT.load(Ordering::Relaxed)
    }
}

#[cfg(feature = "bench-alloc")]
#[global_allocator]
static GLOBAL: alloc_tracker::Counting = alloc_tracker::Counting;

#[cfg(feature = "bench-alloc")]
fn mib(b: usize) -> f64 {
    b as f64 / 1048576.0
}

// ── Pipeline (mirror of bin/src/main.rs) ──────────────────────────────────────

fn collect_yang_files(dir: &PathBuf, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_yang_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("yang") {
            out.push(path);
        }
    }
}

/// Parse all files in parallel (mirrors main.rs's `par_iter` parse).
fn parse_all(files: &[PathBuf]) -> Vec<(ModuleKey, ast::Stmt)> {
    let mut mods: Vec<(ModuleKey, ast::Stmt)> = files
        .par_iter()
        .filter_map(|path| {
            let src = std::fs::read_to_string(path).ok()?;
            let file_arc: Arc<str> = Arc::from(path.to_string_lossy().as_ref());
            let (stmts, _errs) = yangest_core::parser::parse_yang(&src, Arc::clone(&file_arc));
            let stmt = stmts.into_iter().next()?;
            let name = stmt.arg.clone().unwrap_or_default();
            let revision = stmt
                .get_substmt(ast::BuiltInKeyword::Revision)
                .and_then(|r| r.arg.clone());
            Some((ModuleKey::new(name, revision), stmt))
        })
        .collect();
    let mut seen = HashSet::new();
    mods.retain(|(k, _)| seen.insert(k.name.clone()));
    mods
}

/// Compile parsed modules into a registry, wave by wave, each wave in parallel
/// (mirrors main.rs).
fn compile_all(modules: Vec<(ModuleKey, ast::Stmt)>) -> ModuleRegistry {
    let mut errs = Vec::new();
    let dep_graph = DepGraph::build(&modules, &mut errs);
    let waves = dep_graph.topo_sort_levels().unwrap_or_default();

    let input_refs: Vec<&(ModuleKey, ast::Stmt)> = modules.iter().collect();
    let dev_index = DeviationIndex::build_from_refs(&input_refs);
    let ann_index = AnnotationIndex::build(&modules, &[]);
    let ast_ann_index = AstAnnotationIndex::build(&modules, &[]);

    let mut stmt_map: HashMap<ModuleKey, ast::Stmt> = modules.into_iter().collect();
    let mut reg = ModuleRegistry::new();
    for wave in waves {
        let wave_work: Vec<(ModuleKey, ast::Stmt)> = wave
            .into_iter()
            .filter_map(|key| stmt_map.remove(&key).map(|s| (key, s)))
            .collect();
        let compiled_wave: Vec<CompiledModule> = wave_work
            .into_par_iter()
            .map(|(key, stmt)| {
                let stmt = ast_ann_index.apply(stmt, &key.name);
                compile_module(&key, stmt, &reg, &dev_index, &ann_index, &ast_ann_index)
            })
            .collect();
        for c in compiled_wave {
            reg.insert(Arc::new(c));
        }
    }
    reg
}

// ── Walk driver ───────────────────────────────────────────────────────────────

#[derive(Default)]
struct Counter {
    events: u64,
}

impl TypeResolutionObserver for Counter {
    fn on_typedef_resolved(&mut self, _m: &str, _t: &Typedef) {
        self.events += 1;
    }
    fn on_leafref_resolved(&mut self, _m: &str, _t: &SchemaNode) {
        self.events += 1;
    }
    fn on_constraint_source(&mut self, _m: &str, _k: ConstraintKind, _n: &SchemaNode) {
        self.events += 1;
    }
    fn on_extension_attached(&mut self, _m: &str, _e: &ExtensionInstance, _n: &SchemaNode) {
        self.events += 1;
    }
}

fn main() {
    let Some(dir) = std::env::var_os("YANGEST_BENCH_MODULES") else {
        eprintln!("walker_probe: set YANGEST_BENCH_MODULES to a directory of .yang modules.");
        return;
    };
    let dir = PathBuf::from(dir);
    let limit: Option<usize> = std::env::var("YANGEST_PROBE_LIMIT").ok().and_then(|s| s.parse().ok());
    if let Some(j) = std::env::var("YANGEST_JOBS").ok().and_then(|s| s.parse::<usize>().ok()) {
        let _ = rayon::ThreadPoolBuilder::new().num_threads(j).build_global();
    }

    let mut files = Vec::new();
    collect_yang_files(&dir, &mut files);
    files.sort();
    if files.is_empty() {
        eprintln!("walker_probe: no .yang under {}", dir.display());
        return;
    }
    eprintln!(
        "walker_probe: {} files under {} ({} rayon threads)",
        files.len(),
        dir.display(),
        rayon::current_num_threads()
    );

    let t0 = Instant::now();
    let modules = parse_all(&files);
    let reg = compile_all(modules);
    eprintln!(
        "walker_probe: compiled {} modules in {:.1}s",
        reg.modules.len(),
        t0.elapsed().as_secs_f64()
    );
    #[cfg(feature = "bench-alloc")]
    eprintln!(
        "walker_probe: live after compile ≈ {:.0} MiB (peak ≈ {:.0} MiB)",
        mib(alloc_tracker::current_bytes()),
        mib(alloc_tracker::peak_bytes())
    );

    // Shared cross-module expand cache, exactly as bin/src/main.rs builds for emit.
    let shared_expand_cache: Arc<RwLock<HashMap<(usize, String), Arc<Vec<SchemaNode>>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let features: HashSet<(String, String)> = HashSet::new();

    let module_arcs: Vec<Arc<CompiledModule>> = reg.modules.values().cloned().collect();
    let to_walk = limit.unwrap_or(module_arcs.len()).min(module_arcs.len());
    let slice = &module_arcs[..to_walk];

    let total_events = AtomicU64::new(0);
    let t1 = Instant::now();
    // Walk every module in parallel — one ExpansionCtx per worker, all sharing
    // the cross-module expand cache (the real emit path's contract).
    slice.par_iter().for_each(|module| {
        let types = TypeRegistry::new(&reg);
        let ctx = ExpansionCtx::new(&reg, &features)
            .with_unlisted_modules_enabled()
            .with_shared_expand_cache(Arc::clone(&shared_expand_cache));
        let walker = SchemaWalker::new(module, &reg, &types, &ctx);
        let mut counter = Counter::default();
        walker.walk(&mut counter);
        total_events.fetch_add(counter.events, Ordering::Relaxed);
    });

    eprintln!(
        "walker_probe: walked {} modules, {} events, in {:.1}s",
        to_walk,
        total_events.load(Ordering::Relaxed),
        t1.elapsed().as_secs_f64()
    );
    #[cfg(feature = "bench-alloc")]
    eprintln!(
        "walker_probe: PEAK live allocation ≈ {:.0} MiB",
        mib(alloc_tracker::peak_bytes())
    );
}
