// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Dependency output plugin.

use std::collections::HashSet;
use std::io::Write;
use std::sync::Arc;

use clap::{Arg, ArgMatches};
use yangest_core::compiler::{CompiledModule, ExpansionCtx, ModuleRegistry};
use yangest_core::plugin::{EmitState, Plugin};

/// Options for the depend plugin (the `--depend-*` flags).
#[derive(Debug, Clone, Default)]
pub struct DependOptions {
    /// Exclude submodule (include) dependencies from output.
    pub no_submodules: bool,
    /// Recurse into transitive dependencies.
    pub recurse: bool,
    /// Module names to omit from the dependency list.
    pub ignore_modules: Vec<String>,
    /// Extension to append to dependency names (e.g. ".test"). When combined
    /// with `include_path`, this overrides the default ".yang" extension.
    pub extension: Option<String>,
    /// Use filenames (name + ".yang" or `extension`) as dependency names
    /// instead of bare module names.
    pub include_path: bool,
    /// Use "::" instead of ":" as the separator.
    pub double_colon: bool,
    /// Override the target name (left-hand side of the rule).
    pub target: Option<String>,
}

pub struct DependPlugin {
    pub options: DependOptions,
}

impl Default for DependPlugin {
    fn default() -> Self {
        Self {
            options: DependOptions::default(),
        }
    }
}

impl DependPlugin {
    pub fn new(options: DependOptions) -> Self {
        Self { options }
    }

    /// Return the formatted dependency token for `module_name`.
    fn dep_token(&self, module_name: &str) -> String {
        match (&self.options.extension, self.options.include_path) {
            (Some(ext), _) => format!("{}{}", module_name, ext),
            (None, true) => format!("{}.yang", module_name),
            (None, false) => module_name.to_string(),
        }
    }

    /// Collect the (ordered) set of direct dependencies for a module.
    /// Always returns in declaration order (imports first, then includes).
    /// Includes are omitted when `no_submodules` is set.
    fn direct_deps<'a>(&self, module: &'a CompiledModule) -> Vec<&'a str> {
        // Use raw declared imports (not just resolved ones) so that the depend
        // plugin works correctly even when the imported modules were not provided.
        let imports: Vec<&str> = module.imports.iter().map(String::as_str).collect();
        let includes: Vec<&str> = if self.options.no_submodules {
            vec![]
        } else {
            module.includes.iter().map(String::as_str).collect()
        };
        imports.into_iter().chain(includes.into_iter()).collect()
    }

    /// Collect all dependency names for `module`.
    ///
    /// Without `--depend-recurse`: returns direct deps in declaration order.
    /// With `--depend-recurse`: returns all transitive deps, sorted and
    /// deduplicated. Ignored modules are NOT filtered here —
    /// they are filtered at emit time so their transitive deps are still
    /// included.
    fn collect_deps(&self, module: &CompiledModule, registry: &ModuleRegistry) -> Vec<String> {
        if self.options.recurse {
            let mut all: HashSet<String> = HashSet::new();
            self.collect_recursive(module, registry, &mut HashSet::new(), &mut all);
            let mut result: Vec<String> = all.into_iter().collect();
            result.sort();
            result
        } else {
            let mut result: Vec<String> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for name in self.direct_deps(module) {
                if seen.insert(name.to_string()) {
                    result.push(name.to_string());
                }
            }
            result
        }
    }

    /// DFS helper for recursive dependency collection.
    /// Ignored modules are still traversed so their transitive deps are found.
    fn collect_recursive(
        &self,
        module: &CompiledModule,
        registry: &ModuleRegistry,
        in_progress: &mut HashSet<String>,
        all_deps: &mut HashSet<String>,
    ) {
        for name in self.direct_deps(module) {
            let name = name.to_string();
            if all_deps.insert(name.clone()) && !in_progress.contains(&name) {
                in_progress.insert(name.clone());
                if let Some(dep_mod) = registry.resolve_import(&name, None) {
                    self.collect_recursive(&dep_mod, registry, in_progress, all_deps);
                }
                in_progress.remove(&name);
            }
        }
    }
}

impl Plugin for DependPlugin {
    fn name(&self) -> &'static str {
        "depend"
    }

    fn cli_args(&self) -> Vec<Arg> {
        vec![
            Arg::new("depend-no-submodules")
                .long("depend-no-submodules")
                .action(clap::ArgAction::SetTrue)
                .help("Exclude submodule (include) dependencies from output"),
            Arg::new("depend-recurse")
                .long("depend-recurse")
                .action(clap::ArgAction::SetTrue)
                .help("Include transitive (recursive) dependencies"),
            Arg::new("depend-ignore-module")
                .long("depend-ignore-module")
                .value_name("MODULE")
                .action(clap::ArgAction::Append)
                .help("Omit MODULE from the dependency list (may be repeated)"),
            Arg::new("depend-extension")
                .long("depend-extension")
                .value_name("EXT")
                .help("Append EXT to each dependency name (e.g. \".yang\")"),
            Arg::new("depend-include-path")
                .long("depend-include-path")
                .action(clap::ArgAction::SetTrue)
                .help("Use filenames (name + \".yang\") instead of bare module names"),
            Arg::new("depend-double-colon")
                .long("depend-double-colon")
                .action(clap::ArgAction::SetTrue)
                .help("Use \"::\" instead of \":\" as the target/dep separator"),
            Arg::new("depend-target")
                .long("depend-target")
                .value_name("NAME")
                .help("Override the make target name (left-hand side of the rule)"),
        ]
    }

    fn configure(&mut self, matches: &ArgMatches) {
        self.options.no_submodules = matches.get_flag("depend-no-submodules");
        self.options.recurse = matches.get_flag("depend-recurse");
        self.options.ignore_modules = matches
            .get_many::<String>("depend-ignore-module")
            .map(|v| v.cloned().collect())
            .unwrap_or_default();
        self.options.extension = matches
            .get_one::<String>("depend-extension")
            .cloned();
        self.options.include_path = matches.get_flag("depend-include-path");
        self.options.double_colon = matches.get_flag("depend-double-colon");
        self.options.target = matches.get_one::<String>("depend-target").cloned();
    }

    fn emit_module(
        &self,
        module: &Arc<CompiledModule>,
        registry: &ModuleRegistry,
        _ctx: &ExpansionCtx<'_>,
        _bundle: &yangest_core::plugin::BundleState,
        _state: &mut EmitState,
        out: &mut dyn Write,
    ) -> std::io::Result<()> {
        let target = if let Some(t) = &self.options.target {
            t.clone()
        } else if let Some(path) = &module.source_path {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("{}.yang", module.key.name))
        } else {
            format!("{}.yang", module.key.name)
        };

        let deps = self.collect_deps(module, registry);
        let ignore: HashSet<&str> = self
            .options
            .ignore_modules
            .iter()
            .map(String::as_str)
            .collect();
        let sep = if self.options.double_colon { "::" } else { ":" };

        write!(out, "{} {}", target, sep)?;
        for dep in &deps {
            if !ignore.contains(dep.as_str()) {
                write!(out, " {}", self.dep_token(dep))?;
            }
        }
        writeln!(out)?;
        Ok(())
    }
}

inventory::submit! {
    yangest_core::plugin::PluginRegistration { factory: || Box::new(DependPlugin::default()) }
}
