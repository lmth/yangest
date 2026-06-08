// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! `yangest generate-bundle <DIR>...` — scaffold a `.yangbundle` from a directory
//! tree of YANG files.
//!
//! The tree is walked recursively. Every top-level `module` becomes a primary
//! module (emitted) unless it is classified as a deviation module (has top-level
//! `deviation` statements) or an annotation module (uses a plugin-declared overlay
//! extension). `submodule` files are not listed as primary targets; instead the
//! directories that contain them are added to `search_paths` so `include`
//! resolution finds them. Dependency-only directories passed with `-p` are carried
//! over verbatim into `search_paths`.
//!
//! Roles are inferred from file *contents*, with one deliberate exception the user
//! opts into: every plain module in the tree is treated as primary. The one role
//! that genuinely cannot be inferred — primary target vs. dependency-only module —
//! is therefore resolved by convention (tree = primary, `-p` = dependency). The
//! result is a scaffold to review, not a final answer.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use clap::Parser;

use yangest_core::ast::{self, BuiltInKeyword, Keyword, ModuleKey};
use yangest_core::plugin::{Plugin, PluginRegistration};

#[derive(Parser, Debug)]
#[command(
    name = "yangest generate-bundle",
    about = "Scaffold a .yangbundle by classifying a directory tree of YANG files"
)]
pub struct GenBundleArgs {
    /// Root directory to scan recursively for primary YANG modules (repeatable).
    #[arg(value_name = "DIR", required = true)]
    roots: Vec<PathBuf>,

    /// Dependency-only search-path directory, carried into the bundle's
    /// `search_paths` (repeatable). Modules here are resolved for imports but
    /// never emitted.
    #[arg(short = 'p', long = "path", value_name = "DIR")]
    paths: Vec<PathBuf>,

    /// Write the bundle to FILE instead of stdout. Paths in the bundle are made
    /// relative to FILE's directory.
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    output: Option<PathBuf>,
}

/// (module, extension-name) pairs that mark a file as an annotation module,
/// gathered from every registered plugin's overlay-extension declarations.
fn annotation_extension_ids() -> BTreeSet<(String, String)> {
    let mut set = BTreeSet::new();
    for reg in inventory::iter::<PluginRegistration> {
        let plugin: Box<dyn Plugin> = (reg.factory)();
        for ov in plugin.overlay_extensions() {
            set.insert((ov.module.to_string(), ov.name.to_string()));
        }
        for ao in plugin.ast_overlay_extensions() {
            set.insert((ao.module_selector.module.to_string(), ao.module_selector.name.to_string()));
            set.insert((ao.stmt_selector.module.to_string(), ao.stmt_selector.name.to_string()));
        }
    }
    set
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Primary,
    Deviation,
    Annotation,
    Submodule,
}

/// Classify a parsed module header statement.
fn classify(header: &ast::Stmt, ann_exts: &BTreeSet<(String, String)>) -> Option<Role> {
    match &header.keyword {
        Keyword::BuiltIn(BuiltInKeyword::Submodule) => Some(Role::Submodule),
        Keyword::BuiltIn(BuiltInKeyword::Module) => {
            if header.get_substmts(BuiltInKeyword::Deviation).next().is_some() {
                return Some(Role::Deviation);
            }
            let prefix_to_module = import_prefix_map(header);
            if uses_annotation_ext(header, &prefix_to_module, ann_exts) {
                return Some(Role::Annotation);
            }
            Some(Role::Primary)
        }
        _ => None,
    }
}

/// Map each imported prefix to the module name it refers to (`import M { prefix p; }`).
fn import_prefix_map(header: &ast::Stmt) -> Vec<(String, String)> {
    let mut map = Vec::new();
    for imp in header.get_substmts(BuiltInKeyword::Import) {
        let Some(module) = imp.arg.as_deref() else { continue };
        if let Some(pfx) = imp.get_substmt(BuiltInKeyword::Prefix).and_then(|p| p.arg.as_deref()) {
            map.push((pfx.to_string(), module.to_string()));
        }
    }
    map
}

/// True if any statement in the tree uses an extension keyword that resolves
/// (via the module's imports) to one of the registered annotation extensions.
fn uses_annotation_ext(
    stmt: &ast::Stmt,
    prefix_to_module: &[(String, String)],
    ann_exts: &BTreeSet<(String, String)>,
) -> bool {
    if let Keyword::ExtensionPrefixed { prefix, name } = &stmt.keyword {
        if let Some((_, module)) = prefix_to_module.iter().find(|(p, _)| p == prefix) {
            if ann_exts.contains(&(module.clone(), name.clone())) {
                return true;
            }
        }
    }
    stmt.substmts
        .iter()
        .any(|s| uses_annotation_ext(s, prefix_to_module, ann_exts))
}

/// True if the module declares at least one top-level data-definition node — used
/// only to flag deviation modules that also carry their own data tree as ambiguous.
fn has_data_nodes(header: &ast::Stmt) -> bool {
    use BuiltInKeyword::*;
    header.substmts.iter().any(|s| {
        matches!(
            &s.keyword,
            Keyword::BuiltIn(
                Container | Leaf | LeafList | List | Choice | AnyXml | AnyData | Uses
                    | Rpc | Notification | Augment
            )
        )
    })
}

/// Insert a `name -> (revision, path)` mapping, keeping the latest revision and
/// recording a `# note:` for whichever revision is dropped (RFC 7950 §5.1.1).
fn insert_latest(
    map: &mut HashMap<String, (Option<String>, PathBuf)>,
    name: String,
    rev: Option<String>,
    rel: PathBuf,
    notes: &mut Vec<String>,
) {
    match map.get(&name) {
        Some((cur_rev, cur_path)) => {
            if ModuleKey::revision_cmp(cur_rev.as_deref(), rev.as_deref())
                == std::cmp::Ordering::Less
            {
                notes.push(format!("'{name}': {} supersedes {}", rel.display(), cur_path.display()));
                map.insert(name, (rev, rel));
            } else {
                notes.push(format!("'{name}': {} is an older revision, not listed", rel.display()));
            }
        }
        None => {
            map.insert(name, (rev, rel));
        }
    }
}

fn collect_yang_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_yang_files(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("yang") {
            out.push(p);
        }
    }
}

/// Make `target` relative to `base` (both made absolute first). Falls back to the
/// absolute target if either cannot be canonicalised.
fn relativize(base: &Path, target: &Path) -> PathBuf {
    let (Ok(base), Ok(target)) = (abs(base), abs(target)) else {
        return target.to_path_buf();
    };
    let bc: Vec<_> = base.components().collect();
    let tc: Vec<_> = target.components().collect();
    let mut i = 0;
    while i < bc.len() && i < tc.len() && bc[i] == tc[i] {
        i += 1;
    }
    let mut out = PathBuf::new();
    for _ in i..bc.len() {
        out.push("..");
    }
    for c in &tc[i..] {
        out.push(c.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

fn abs(p: &Path) -> io::Result<PathBuf> {
    std::fs::canonicalize(p)
}

fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn write_array(buf: &mut String, key: &str, items: &BTreeSet<PathBuf>) {
    if items.is_empty() {
        buf.push_str(&format!("{key} = []\n\n"));
        return;
    }
    buf.push_str(&format!("{key} = [\n"));
    for p in items {
        buf.push_str(&format!("    \"{}\",\n", toml_escape(&p.to_string_lossy())));
    }
    buf.push_str("]\n\n");
}

pub fn run() -> ! {
    // argv[1] is "generate-bundle"; let clap parse from there so --help works.
    let args = GenBundleArgs::parse_from(std::env::args().skip(1));
    match generate(&args) {
        Ok(out) => {
            if let Some(path) = &args.output {
                if let Err(e) = std::fs::write(path, &out) {
                    eprintln!("yangest generate-bundle: cannot write '{}': {e}", path.display());
                    std::process::exit(1);
                }
                eprintln!("yangest generate-bundle: wrote {}", path.display());
            } else {
                print!("{out}");
            }
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("yangest generate-bundle: {e}");
            std::process::exit(1);
        }
    }
}

fn generate(args: &GenBundleArgs) -> Result<String, String> {
    let ann_exts = annotation_extension_ids();

    // Directory the bundle's relative paths are anchored to.
    let bundle_dir: PathBuf = match &args.output {
        Some(o) => o.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf),
        None => None,
    }
    .unwrap_or_else(|| PathBuf::from("."));

    let mut files = Vec::new();
    for root in &args.roots {
        if !root.is_dir() {
            return Err(format!("'{}' is not a directory", root.display()));
        }
        collect_yang_files(root, &mut files);
    }
    files.sort();
    files.dedup();
    if files.is_empty() {
        return Err("no .yang files found under the given directory tree".into());
    }

    // Each bucket keeps the latest revision per module name; a directory may
    // hold several revisions of the same module (RFC 7950 §5.2 `name@date.yang`).
    let mut primary: HashMap<String, (Option<String>, PathBuf)> = HashMap::new();
    let mut deviation: HashMap<String, (Option<String>, PathBuf)> = HashMap::new();
    let mut annotation: HashMap<String, (Option<String>, PathBuf)> = HashMap::new();
    let mut search_paths = BTreeSet::new();
    let mut notes: Vec<String> = Vec::new();
    let mut skipped = 0usize;

    for file in &files {
        let Ok(src) = std::fs::read_to_string(file) else {
            skipped += 1;
            continue;
        };
        let file_arc: std::sync::Arc<str> = std::sync::Arc::from(file.to_string_lossy().as_ref());
        let (stmts, _errs) = yangest_core::parser::parse_yang(&src, file_arc);
        let Some(header) = stmts.into_iter().next() else {
            skipped += 1;
            continue;
        };
        let name = header.arg.clone().unwrap_or_default();
        let rev = header
            .get_substmt(BuiltInKeyword::Revision)
            .and_then(|r| r.arg.clone());
        let rel = relativize(&bundle_dir, file);
        match classify(&header, &ann_exts) {
            Some(Role::Primary) => insert_latest(&mut primary, name, rev, rel, &mut notes),
            Some(Role::Deviation) => {
                if has_data_nodes(&header) {
                    notes.push(format!(
                        "{} has both deviations and its own data nodes — review whether it belongs in `modules` instead",
                        rel.display()
                    ));
                }
                insert_latest(&mut deviation, name, rev, rel, &mut notes);
            }
            Some(Role::Annotation) => insert_latest(&mut annotation, name, rev, rel, &mut notes),
            Some(Role::Submodule) => {
                // Not a primary target; ensure its directory is searchable so
                // `include` resolution finds it (latest revision; RFC 7950 §5.1).
                if let Some(parent) = file.parent() {
                    search_paths.insert(relativize(&bundle_dir, parent));
                }
            }
            None => skipped += 1,
        }
    }

    let modules: BTreeSet<PathBuf> = primary.into_values().map(|(_, p)| p).collect();
    let deviation_modules: BTreeSet<PathBuf> = deviation.into_values().map(|(_, p)| p).collect();
    let annotation_modules: BTreeSet<PathBuf> = annotation.into_values().map(|(_, p)| p).collect();

    // Dependency directories given with -p are carried over verbatim.
    for p in &args.paths {
        search_paths.insert(relativize(&bundle_dir, p));
    }

    let mut buf = String::new();
    buf.push_str("# Generated by `yangest generate-bundle`. This is a scaffold — review it.\n");
    buf.push_str("# Every plain module found in the scanned tree is listed as a primary\n");
    buf.push_str("# (emitted) module. If some of these are dependency-only, move them into a\n");
    buf.push_str("# directory referenced by `search_paths` instead. Deviation and annotation\n");
    buf.push_str("# modules were classified by content; submodule directories were added to\n");
    buf.push_str("# `search_paths` so `include` resolves.\n");
    for note in &notes {
        buf.push_str(&format!("# note: {note}\n"));
    }
    if skipped > 0 {
        buf.push_str(&format!("# ({skipped} file(s) skipped: unreadable or unparsable)\n"));
    }
    buf.push('\n');

    write_array(&mut buf, "modules", &modules);
    write_array(&mut buf, "search_paths", &search_paths);
    write_array(&mut buf, "deviation_modules", &deviation_modules);
    write_array(&mut buf, "annotation_modules", &annotation_modules);

    buf.push_str("# Enable specific features per module here if needed, e.g.:\n");
    buf.push_str("# [features]\n");
    buf.push_str("# \"my-module\" = [\"feature-a\", \"feature-b\"]\n");

    Ok(buf)
}
