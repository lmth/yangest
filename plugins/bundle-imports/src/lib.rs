// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Non-local (cross-bundle) import analysis for a collection of YANG modules.
//!
//! A *bundle* is any collection of YANG files (a directory, a tar archive, a
//! `.yangbundle`, …). Within a bundle, a module's `import` may resolve to another
//! module of the same bundle — that is a **local** import and is not interesting
//! for cross-bundle dependency analysis. An `import` whose target module is *not*
//! defined anywhere in the bundle is a **non-local** import: the dependency must
//! be satisfied by some other bundle.
//!
//! This crate computes, for one bundle, the modules it provides and its non-local
//! imports. It is deliberately resolution-by-name and bundle-local: because two
//! bundles can define the same module name (or namespace) in different ways, an
//! import is classified purely against *this* bundle's own modules — never
//! resolved globally here. Stitching non-local imports across bundles into a
//! dependency graph is the job of a downstream consumer (which knows, for every
//! bundle, what it [`provides`](BundleReport::provides)).
//!
//! Two entry points:
//! - [`analyze_sources`] / [`analyze_headers`] — library API, used by a tool that
//!   has the YANG text in hand (e.g. extracted from a tar or a directory).
//! - the registered [`Plugin`] (`-f bundle-imports`) — emits the same
//!   [`BundleReport`] as JSON for whatever module set yangest was given.

use std::collections::HashSet;
use std::io::Write;
use std::sync::Arc;

use clap::{Arg, ArgAction, ArgMatches};
use serde::{Deserialize, Serialize};
use yangest_core::ast::{BuiltInKeyword, Stmt};
use yangest_core::compiler::{CompiledModule, ExpansionCtx, ModuleRegistry};
use yangest_core::parser::parse_yang;
use yangest_core::plugin::Plugin;

/// A module (or submodule) defined by the bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvidedModule {
    /// Module name (the `import`-able identifier).
    pub name: String,
    /// `namespace` value (`None` for submodules). Carried so a consumer can flag
    /// the same module name being provided by two bundles with *different*
    /// namespaces — the "same name defined differently" hazard.
    pub namespace: Option<String>,
    /// Latest `revision` date, if any.
    pub revision: Option<String>,
    /// True if this is a `submodule` rather than a `module`.
    pub submodule: bool,
}

/// An `import` whose target module is not defined within the bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NonLocalImport {
    /// The module (in this bundle) that declares the import.
    pub from: String,
    /// The imported module name (not satisfied locally).
    pub import: String,
    /// The `revision-date` on the import, if pinned.
    pub revision: Option<String>,
}

/// The non-local-import report for one bundle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleReport {
    /// Every module/submodule the bundle defines (the local resolution set).
    pub provides: Vec<ProvidedModule>,
    /// Imports not satisfied within the bundle, in declaration order.
    pub non_local_imports: Vec<NonLocalImport>,
}

impl BundleReport {
    /// The set of module names this bundle provides.
    pub fn provided_names(&self) -> HashSet<&str> {
        self.provides.iter().map(|p| p.name.as_str()).collect()
    }
}

/// Analyse a set of already-parsed module/submodule headers.
///
/// Accepts the top-level `Stmt` of each file (a `module` or `submodule`). Other
/// statements are ignored, so passing whole parsed files is fine.
pub fn analyze_headers<'a>(headers: impl IntoIterator<Item = &'a Stmt>) -> BundleReport {
    let mut provides = Vec::new();
    let mut raw_imports: Vec<NonLocalImport> = Vec::new();

    for header in headers {
        let is_module = header.keyword.is_builtin(BuiltInKeyword::Module);
        let is_submodule = header.keyword.is_builtin(BuiltInKeyword::Submodule);
        if !is_module && !is_submodule {
            continue;
        }
        let Some(name) = header.arg.clone() else {
            continue;
        };
        provides.push(ProvidedModule {
            name: name.clone(),
            namespace: header
                .get_substmt(BuiltInKeyword::Namespace)
                .and_then(|s| s.arg.clone()),
            revision: latest_revision(header),
            submodule: is_submodule,
        });
        for imp in header.get_substmts(BuiltInKeyword::Import) {
            if let Some(target) = imp.arg.clone() {
                raw_imports.push(NonLocalImport {
                    from: name.clone(),
                    import: target,
                    revision: imp
                        .get_substmt(BuiltInKeyword::RevisionDate)
                        .and_then(|s| s.arg.clone()),
                });
            }
        }
    }

    // An import is non-local iff its target is not a module this bundle defines.
    // Resolution is by name and strictly bundle-local (see the module docs).
    let provided: HashSet<&str> = provides.iter().map(|p| p.name.as_str()).collect();
    let non_local_imports = raw_imports
        .into_iter()
        .filter(|imp| !provided.contains(imp.import.as_str()))
        .collect();

    BundleReport { provides, non_local_imports }
}

/// Parse `(filename, contents)` YANG sources and analyse them as one bundle.
///
/// Parse errors are ignored: the header (name + imports) of a file is almost
/// always recoverable even when the body has issues, and this analysis only
/// needs the header.
pub fn analyze_sources<S: AsRef<str>>(sources: &[(String, S)]) -> BundleReport {
    let parsed: Vec<Stmt> = sources
        .iter()
        .flat_map(|(file, content)| {
            let (stmts, _errors) = parse_yang(content.as_ref(), Arc::from(file.as_str()));
            stmts
        })
        .collect();
    analyze_headers(parsed.iter())
}

/// Latest `revision` date declared in a module header, if any.
fn latest_revision(header: &Stmt) -> Option<String> {
    header
        .get_substmts(BuiltInKeyword::Revision)
        .filter_map(|r| r.arg.clone())
        .max()
}

/// The `-f bundle-imports` plugin: emit the [`BundleReport`] for the module set
/// yangest was given, as pretty JSON.
///
/// Also demonstrates the plugin **CLI-option** and **diagnostics** mechanisms:
/// the `--bundle-imports-warn-unpinned` flag (declared in [`cli_args`], consumed
/// in [`configure`]) makes the plugin raise a *warning* for every `import` that
/// is not revision-pinned. Under `yangest --werror` those warnings fail the run.
#[derive(Default)]
pub struct BundleImportsPlugin {
    warn_unpinned: bool,
}

impl Plugin for BundleImportsPlugin {
    fn name(&self) -> &'static str {
        "bundle-imports"
    }

    fn extension(&self) -> &'static str {
        "bundle-imports.json"
    }

    fn cli_args(&self) -> Vec<Arg> {
        vec![
            Arg::new("bundle-imports-warn-unpinned")
                .long("bundle-imports-warn-unpinned")
                .action(ArgAction::SetTrue)
                .help("Warn for each import that lacks a revision-date (lint)"),
        ]
    }

    fn configure(&mut self, matches: &ArgMatches) {
        self.warn_unpinned = matches.get_flag("bundle-imports-warn-unpinned");
    }

    /// Whole-bundle output: one JSON document for the entire module set, not one
    /// per module. Uses each compiled module's retained header `stmt`, so it works
    /// even when cross-bundle imports leave the bundle un-resolvable.
    fn emit(
        &self,
        modules: &[Arc<CompiledModule>],
        _registry: &ModuleRegistry,
        ctx: &ExpansionCtx<'_>,
        out: &mut dyn Write,
    ) -> std::io::Result<()> {
        if self.warn_unpinned {
            if let Some(diags) = ctx.diagnostics() {
                for header in modules.iter().map(|m| &m.stmt) {
                    let from = header.arg.clone().unwrap_or_default();
                    for imp in header.get_substmts(BuiltInKeyword::Import) {
                        let pinned = imp.get_substmt(BuiltInKeyword::RevisionDate).is_some();
                        if let (false, Some(target)) = (pinned, imp.arg.as_deref()) {
                            diags.warning_with_code(
                                self.name(),
                                "unpinned-import",
                                format!("{from}: import of '{target}' is not revision-pinned"),
                            );
                        }
                    }
                }
            }
        }
        let report = analyze_headers(modules.iter().map(|m| &m.stmt));
        let json = serde_json::to_string_pretty(&report)
            .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
        writeln!(out, "{json}")
    }
}

inventory::submit! {
    yangest_core::plugin::PluginRegistration {
        factory: || Box::new(BundleImportsPlugin::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(name: &str, body: &str) -> (String, String) {
        (format!("{name}.yang"), body.to_string())
    }

    #[test]
    fn classifies_local_vs_non_local_imports() {
        // Bundle defines `a` and `b`. `a` imports `b` (local) and `ietf-x`
        // (non-local). `b` imports `a` (local).
        let sources = vec![
            src(
                "a",
                r#"module a {
                    namespace "urn:a"; prefix a;
                    import b { prefix b; }
                    import ietf-x { prefix x; revision-date 2020-01-01; }
                }"#,
            ),
            src(
                "b",
                r#"module b {
                    namespace "urn:b"; prefix b;
                    import a { prefix a; }
                }"#,
            ),
        ];
        let report = analyze_sources(&sources);

        let provided = report.provided_names();
        assert!(provided.contains("a") && provided.contains("b"));

        // Only the import of `ietf-x` is non-local.
        assert_eq!(report.non_local_imports.len(), 1);
        let nl = &report.non_local_imports[0];
        assert_eq!(nl.from, "a");
        assert_eq!(nl.import, "ietf-x");
        assert_eq!(nl.revision.as_deref(), Some("2020-01-01"));
    }

    #[test]
    fn submodule_include_is_local_and_namespace_recorded() {
        let sources = vec![
            src(
                "main",
                r#"module main {
                    namespace "urn:main"; prefix m;
                    include main-sub;
                    import other { prefix o; }
                    revision 2021-05-05;
                    revision 2019-01-01;
                }"#,
            ),
            src(
                "main-sub",
                r#"submodule main-sub {
                    belongs-to main { prefix m; }
                }"#,
            ),
        ];
        let report = analyze_sources(&sources);

        // `other` is the only non-local dependency; the submodule is provided.
        assert_eq!(report.non_local_imports.len(), 1);
        assert_eq!(report.non_local_imports[0].import, "other");

        let main = report.provides.iter().find(|p| p.name == "main").unwrap();
        assert_eq!(main.namespace.as_deref(), Some("urn:main"));
        assert_eq!(main.revision.as_deref(), Some("2021-05-05")); // latest of the two
        let sub = report.provides.iter().find(|p| p.name == "main-sub").unwrap();
        assert!(sub.submodule);
    }
}
