// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Performance baseline for heavy bundle builds of tree-plugin output.
//!
//! This benchmark is the verification mechanism for the project's hard
//! performance constraint: adding the missing core features must not regress
//! the wall-clock time or peak memory of a heavy tree-plugin bundle build.
//!
//! It does NOT ship fixtures (the repo only carries a handful of small IETF
//! modules under `modules/`). Point it at a heavy module tree via the
//! `YANGEST_BENCH_MODULES` environment variable:
//!
//! ```sh
//! YANGEST_BENCH_MODULES=/path/to/yang-modules \
//!     cargo bench --bench bundle_tree
//! # with peak-allocation tracking:
//! YANGEST_BENCH_MODULES=/path/to/yang-modules \
//!     cargo bench --bench bundle_tree --features bench-alloc
//! ```
//!
//! Two criterion groups mirror the real pipeline in `bin/src/main.rs`, and now
//! run BOTH phases in parallel (rayon) exactly as `main.rs` does — parse and
//! per-wave compile in `compile_all`, per-module emit in `emit_tree`. The
//! earlier version was single-threaded, which hid the real bottleneck (emit
//! does not scale past ~2.5 cores — see below).
//!   * `compile_only` — parse every module and compile into a `ModuleRegistry`.
//!   * `tree_emit`    — the constraint-critical path: build an `ExpansionCtx`
//!                      with a shared expand cache (exactly as `main.rs`) and run
//!                      the tree plugin's `emit_module` for every module.
//!
//! BASELINE — 2026-06-06, 20-core host, Cisco IOS-XE bundle (~1003–1071 .yang).
//! Cross-check via the real binary (authoritative, parse+compile+emit+write),
//! full tree build at j=20, showing the two landed optimizations stacking:
//!   glibc malloc, Vec children      : 2.94 s / 2.09 GB   (original)
//!   + mimalloc global allocator     : 1.34 s / 1.81 GB
//!   + Arc-shared SchemaNode children: 0.67 s / 0.92 GB   ← current (4.4× / 2.3×)
//! Why: callgrind showed ~50% of emit in libc malloc/free deep-cloning expanded
//! SchemaNode subtrees. mimalloc cut allocator cost; Arc<Vec> children made
//! SchemaNode::clone shallow (copy-on-write via Arc::make_mut), removing the
//! churn at its source — halving both time and peak RSS.
//! Residual: parallel scaling is ~1.6× (j1 1.06s → j20 0.67s) — load imbalance
//! from one giant module (Cisco-IOS-XE-native = 58% of output), NOT lock/alloc
//! contention. Only intra-module parallelism would lift that.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use criterion::{black_box, Criterion};
use rayon::prelude::*;

use yangest_core::annindex::AnnotationIndex;
use yangest_core::astannindex::AstAnnotationIndex;
use yangest_core::ast::{self, ModuleKey};
use yangest_core::compiler::{
    compile_module, CompiledModule, ExpansionCtx, ModuleRegistry, SchemaNode,
};
use yangest_core::depgraph::DepGraph;
use yangest_core::devindex::DeviationIndex;
use yangest_core::plugin::{EmitState, Plugin};
use yangest_plugin_tree::TreePlugin;

// ── Optional peak-allocation tracker (feature `bench-alloc`) ──────────────────

#[cfg(feature = "bench-alloc")]
mod alloc_tracker {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CURRENT: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);

    /// A `System`-wrapping allocator that records the high-water mark of live
    /// bytes. Approximate (per-thread relaxed counters) but reproducible enough
    /// to catch regressions in the ±2% gate.
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
}

#[cfg(feature = "bench-alloc")]
#[global_allocator]
static GLOBAL: alloc_tracker::Counting = alloc_tracker::Counting;

// ── Pipeline (a faithful, minimal mirror of bin/src/main.rs) ──────────────────

/// Recursively collect every `.yang` file under `dir`.
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

/// Parse all files into `(ModuleKey, Stmt)` pairs, deduplicated by module name.
fn parse_all(files: &[PathBuf]) -> Vec<(ModuleKey, ast::Stmt)> {
    // Parse files in parallel (mirrors main.rs). `par_iter().collect()` preserves
    // input order, so the subsequent name-dedup stays deterministic.
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

/// Compile parsed modules into a registry, wave by wave (mirrors main.rs).
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
        // Collect this wave's work, then compile its modules in parallel
        // (mirrors main.rs: `into_par_iter` over each dependency wave, reading the
        // registry of already-compiled lower waves through a shared `&reg`).
        let wave_work: Vec<(ModuleKey, ast::Stmt)> = wave
            .into_iter()
            .filter_map(|key| stmt_map.remove(&key).map(|stmt| (key, stmt)))
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

/// Emit tree output for every module, returning total bytes written.
/// Mirrors the bundle emit path in main.rs: one shared expand cache, a fresh
/// ctx per module, `prepare_bundle` once.
fn emit_tree(reg: &ModuleRegistry) -> usize {
    let plugin = TreePlugin::default();
    let enabled_features: HashSet<(String, String)> = HashSet::new();
    let shared_expand_cache: Arc<RwLock<HashMap<(usize, String), Arc<Vec<SchemaNode>>>>> =
        Arc::new(RwLock::new(HashMap::new()));

    let make_ctx = || {
        ExpansionCtx::new(reg, &enabled_features)
            .with_unlisted_modules_enabled()
            .with_shared_expand_cache(Arc::clone(&shared_expand_cache))
    };

    let modules: Vec<Arc<CompiledModule>> = reg.modules.values().cloned().collect();
    let bundle_ctx = make_ctx();
    let bundle = plugin.prepare_bundle(&modules, reg, &bundle_ctx);

    // Emit every module in parallel (mirrors main.rs: `par_iter` over modules,
    // sharing one expand cache). This is where concurrent expansion of the same
    // big shared groupings happens — the case the lazy model is built for.
    modules
        .par_iter()
        .map(|module| {
            let ctx = make_ctx();
            let mut sink: Vec<u8> = Vec::new();
            let mut state = EmitState::new();
            let _ = plugin.emit_module(module, reg, &ctx, &bundle, &mut state, &mut sink);
            sink.len()
        })
        .sum()
}

fn main() {
    let Some(dir) = std::env::var_os("YANGEST_BENCH_MODULES") else {
        eprintln!(
            "bundle_tree: YANGEST_BENCH_MODULES not set — skipping.\n\
             Set it to a directory of .yang modules, e.g.\n\
             YANGEST_BENCH_MODULES=/path/to/yang-modules cargo bench --bench bundle_tree"
        );
        return;
    };
    let dir = PathBuf::from(dir);

    let mut files = Vec::new();
    collect_yang_files(&dir, &mut files);
    files.sort();
    if files.is_empty() {
        eprintln!("bundle_tree: no .yang files found under {}", dir.display());
        return;
    }
    eprintln!("bundle_tree: {} modules under {}", files.len(), dir.display());

    let modules = parse_all(&files);

    let mut c = Criterion::default().configure_from_args();

    {
        let mut g = c.benchmark_group("bundle_tree");
        let files_ref = &files;
        g.bench_function("compile_only", |b| {
            b.iter(|| {
                let parsed = parse_all(files_ref);
                let reg = compile_all(parsed);
                black_box(reg.modules.len())
            });
        });
        g.finish();
    }

    let reg = compile_all(modules);
    eprintln!("bundle_tree: compiled {} modules", reg.modules.len());

    {
        let mut g = c.benchmark_group("bundle_tree");
        g.bench_function("tree_emit", |b| {
            b.iter(|| black_box(emit_tree(&reg)));
        });
        g.finish();
    }

    c.final_summary();

    #[cfg(feature = "bench-alloc")]
    eprintln!(
        "bundle_tree: peak live allocation ≈ {:.1} MiB",
        alloc_tracker::peak_bytes() as f64 / (1024.0 * 1024.0)
    );
}
