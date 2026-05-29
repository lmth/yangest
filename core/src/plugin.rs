// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! The `Plugin` trait — implemented by all output format plugins.
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use clap::{Arg, ArgMatches};

use crate::compiler::{CompiledModule, ExpansionCtx, ModuleRegistry};
use crate::grammar::ExtensionGrammar;

/// Bundle-level shared state computed once before any per-module emission.
///
/// Created by [`Plugin::prepare_bundle`] before the per-module loop starts and
/// shared (via `Arc`) across all parallel `emit_module` calls.  Because it is
/// shared, values stored here must be `Send + Sync` and the map itself is
/// immutable after `prepare_bundle` returns.
///
/// Plugin authors insert typed entries during `prepare_bundle` and retrieve
/// them (read-only) inside `emit_module`:
///
/// ```rust,ignore
/// #[derive(Default)]
/// struct MyBundleData { ns_cache: HashMap<String, String> }
///
/// fn prepare_bundle(&self, modules, registry, ctx) -> BundleState {
///     let mut bundle = BundleState::new();
///     let data = bundle.get_or_insert::<MyBundleData>();
///     for m in registry.modules.values() {
///         data.ns_cache.insert(m.key.name.clone(), m.namespace.clone());
///     }
///     bundle
/// }
///
/// fn emit_module(&self, module, registry, ctx, bundle, state, out) {
///     if let Some(data) = bundle.get::<MyBundleData>() {
///         // use data.ns_cache …
///     }
/// }
/// ```
pub struct BundleState {
    map: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl BundleState {
    /// Create a new, empty bundle state map.
    pub fn new() -> Self {
        BundleState { map: HashMap::new() }
    }

    /// Return a mutable reference to the value of type `T`, inserting
    /// `T::default()` if none is present.  Call this during `prepare_bundle`.
    pub fn get_or_insert<T: Any + Send + Sync + Default>(&mut self) -> &mut T {
        self.map
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(T::default()))
            .downcast_mut::<T>()
            .expect("BundleState: type mismatch — should never happen")
    }

    /// Return a shared reference to the value of type `T`, or `None` if it
    /// was never inserted.  Call this during `emit_module`.
    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.map.get(&TypeId::of::<T>())?.downcast_ref::<T>()
    }
}

impl Default for BundleState {
    fn default() -> Self { Self::new() }
}

/// Per-primary-module mutable state storage for plugin authors.
///
/// The framework creates one `EmitState` per `emit_module` call and discards
/// it afterwards, so state never leaks between modules — even in parallel
/// emission mode where multiple `emit_module` calls run concurrently.
///
/// Plugin authors store their own typed state using [`get_or_insert`]:
///
/// ```rust,ignore
/// #[derive(Default)]
/// struct MyState { seen: HashSet<String> }
///
/// fn emit_module(&self, module, registry, ctx, bundle, state: &mut EmitState, out) {
///     let s: &mut MyState = state.get_or_insert::<MyState>();
///     s.seen.insert(module.key.name.clone());
/// }
/// ```
///
/// Multiple plugins coexist without collision because each type occupies its
/// own [`TypeId`]-keyed slot.
///
/// [`get_or_insert`]: EmitState::get_or_insert
pub struct EmitState {
    map: HashMap<TypeId, Box<dyn Any + Send>>,
}

impl EmitState {
    /// Create a new, empty state map.
    pub fn new() -> Self {
        EmitState { map: HashMap::new() }
    }

    /// Return a mutable reference to the plugin's state value of type `T`,
    /// inserting `T::default()` if none has been stored yet.
    pub fn get_or_insert<T: Any + Send + Default>(&mut self) -> &mut T {
        self.map
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(T::default()))
            .downcast_mut::<T>()
            .expect("EmitState: type mismatch — should never happen")
    }
}

impl Default for EmitState {
    fn default() -> Self { Self::new() }
}

/// Identifies a YANG extension statement by module name and local name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtensionId {
    /// YANG module that defines this extension (e.g. `"tailf-common"`).
    pub module: &'static str,
    /// Local extension name (e.g. `"annotate-module"`).
    pub name: &'static str,
}

/// Declares a pair of extension statements that implement an AST-level
/// annotation overlay: a *module selector* (e.g. `tailf:annotate-module`) and
/// a *statement selector* (e.g. `tailf:annotate-statement`).
///
/// Registered via [`Plugin::ast_overlay_extensions`].  The
/// [`AstAnnotationIndex`] scans overlay modules for `module_selector`
/// occurrences; their bodies are parsed for `stmt_selector` nesting; the
/// resulting patches are applied to the target module's raw [`Stmt`] tree
/// before `compile_module` is called.
///
/// [`AstAnnotationIndex`]: crate::astannindex::AstAnnotationIndex
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AstOverlayDescriptor {
    /// Extension whose argument is a module/submodule name.
    pub module_selector: ExtensionId,
    /// Extension whose argument is a `keyword[name='value']` selector,
    /// used inside `module_selector` bodies (and nested within itself).
    pub stmt_selector: ExtensionId,
}

/// Describes an extension statement in an overlay module that routes per-node
/// annotations to target schema nodes.
///
/// Registered via [`Plugin::overlay_extensions`].  Any occurrence of this
/// extension in an overlay (`--annotation-module`) file whose argument is an
/// absolute schema-node path is treated as an annotation: the extension's
/// body sub-statements are injected into the target node's `extensions` list
/// at expansion time.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OverlayExtension {
    /// YANG module that defines this extension (e.g. `"tailf-common"`).
    pub module: &'static str,
    /// Local extension name (e.g. `"annotate"`).
    pub name: &'static str,
    /// Name of the plugin that registered this entry (value of
    /// [`Plugin::name`]). Used to tag the resulting annotation with its
    /// originating plugin.
    pub source_plugin: &'static str,
}

/// Registration record submitted by each plugin crate via [`inventory::submit!`].
///
/// Each plugin crate calls:
/// ```rust,ignore
/// inventory::submit! {
///     yangest_core::plugin::PluginRegistration { factory: || Box::new(MyPlugin::default()) }
/// }
/// ```
/// The main binary collects all registrations at startup with
/// `inventory::iter::<PluginRegistration>` — no explicit listing required.
pub struct PluginRegistration {
    pub factory: fn() -> Box<dyn Plugin>,
}

inventory::collect!(PluginRegistration);

/// A yangest output plugin.
///
/// Per-module plugins (tree, depend, yang, yin) override [`emit_module`].
/// All-at-once plugins (swagger) override [`emit`] directly.
/// Both kinds should also override [`extension`] so that `--output-dir` mode
/// can name output files correctly.
pub trait Plugin: Send + Sync {
    fn name(&self) -> &'static str;

    /// File extension (without leading dot) used when writing output files
    /// in `--output-dir` batch mode.  Defaults to the plugin name, which is
    /// a reasonable fallback (e.g. "tree", "yang", "yin").  Override when
    /// the format has a canonical extension that differs (e.g. "json" for
    /// swagger, "xml" for an XML format).
    fn extension(&self) -> &'static str {
        self.name()
    }

    /// CLI arguments declared by this plugin.
    ///
    /// Argument IDs and long names should be prefixed with the plugin name
    /// (e.g. `"tree-depth"` / `--tree-depth`) to avoid collisions with core
    /// options and other plugins.  The main binary collects these from all
    /// registered plugins and adds them to the global command, so they appear
    /// in `--help` output alongside the core options.
    ///
    /// Return an empty `Vec` (the default) for plugins without extra options.
    fn cli_args(&self) -> Vec<Arg> {
        vec![]
    }

    /// Called once after CLI parsing so the plugin can extract its own
    /// options from the global [`ArgMatches`].
    ///
    /// The default implementation is a no-op; plugins that declared args in
    /// [`cli_args`](Self::cli_args) should override this to apply them.
    fn configure(&mut self, _matches: &ArgMatches) {}

    /// Overlay extension statements that this plugin wants routed as per-node
    /// annotations.
    ///
    /// For each [`OverlayExtension`] listed here, any occurrence of that
    /// extension statement in an overlay module whose argument is an absolute
    /// schema-node path will have its body sub-statements injected into the
    /// target node's `extensions` list at expansion time.
    ///
    /// Return a `'static` slice (typically a module-level `static` constant):
    ///
    /// ```rust,ignore
    /// static ANN_EXTS: &[OverlayExtension] = &[
    ///     OverlayExtension {
    ///         module: "tailf-common",
    ///         name: "annotate",
    ///         source_plugin: "example-ann",
    ///     },
    /// ];
    /// fn overlay_extensions(&self) -> &'static [OverlayExtension] { ANN_EXTS }
    /// ```
    fn overlay_extensions(&self) -> &'static [OverlayExtension] {
        &[]
    }

    /// AST-level overlay extension pairs implemented by this plugin.
    ///
    /// Each [`AstOverlayDescriptor`] identifies a *module-selector* extension
    /// (takes a module name as argument) and a *statement-selector* extension
    /// (takes a `keyword[name='value']` selector).  Together they implement a
    /// mechanism equivalent to `tailf:annotate-module` + `tailf:annotate-statement`.
    ///
    /// Patches declared via these extensions are applied to the target module's
    /// raw source AST **before** `compile_module` is called, so injected
    /// statements are visible to type resolution, grouping instantiation, and
    /// constraint checking — reaching constructs (groupings, typedefs, the
    /// module statement itself) that have no schema-nodeid.
    ///
    /// ```rust,ignore
    /// static AST_OVERLAYS: &[AstOverlayDescriptor] = &[AstOverlayDescriptor {
    ///     module_selector: ExtensionId { module: "tailf-common", name: "annotate-module" },
    ///     stmt_selector:   ExtensionId { module: "tailf-common", name: "annotate-statement" },
    /// }];
    /// fn ast_overlay_extensions(&self) -> &'static [AstOverlayDescriptor] { AST_OVERLAYS }
    /// ```
    fn ast_overlay_extensions(&self) -> &'static [AstOverlayDescriptor] {
        &[]
    }

    /// Static extension grammar rules declared by this plugin.
    ///
    /// Return a `'static` slice of [`ExtensionGrammar`] entries.  Rules are
    /// collected once at startup into the [`ModuleRegistry`]'s grammar registry
    /// before any module is compiled, so the compiler can validate extension
    /// usage and collect `ExtensionInstance` values on schema nodes.
    ///
    /// Declare rules as a module-level `static` constant:
    ///
    /// ```rust,ignore
    /// static MY_GRAMMAR: &[ExtensionGrammar] = &[
    ///     ExtensionGrammar { module: "acme-ext", name: "purpose", .. },
    /// ];
    /// fn yang_grammar(&self) -> &'static [ExtensionGrammar] { MY_GRAMMAR }
    /// ```
    fn yang_grammar(&self) -> &'static [ExtensionGrammar] {
        &[]
    }

    /// Compute bundle-level shared state once, before any per-module emission.
    ///
    /// Called exactly once before the per-module loop (or before any parallel
    /// workers are spawned).  The returned [`BundleState`] is wrapped in an
    /// `Arc` and passed as `bundle` to every [`emit_module`] call, including
    /// parallel ones, so values stored here must be `Send + Sync`.
    ///
    /// Use this hook to pre-compute data that is:
    /// - **Derived from the full registry or all modules** (e.g. a namespace
    ///   cache built by iterating `registry.modules`), and
    /// - **Expensive enough to matter** when processing hundreds of modules in
    ///   parallel — computing it once here avoids the O(n²) cost of rebuilding
    ///   it inside every `emit_module` call.
    ///
    /// The default implementation returns an empty [`BundleState`]; plugins
    /// with nothing global to pre-compute do not need to override this.
    ///
    /// ```rust,ignore
    /// #[derive(Default)]
    /// struct MyBundle { ns_map: HashMap<String, String> }
    ///
    /// fn prepare_bundle(
    ///     &self,
    ///     _modules: &[Arc<CompiledModule>],
    ///     registry: &ModuleRegistry,
    ///     _ctx: &ExpansionCtx<'_>,
    /// ) -> BundleState {
    ///     let mut bundle = BundleState::new();
    ///     let data = bundle.get_or_insert::<MyBundle>();
    ///     for m in registry.modules.values() {
    ///         data.ns_map.insert(m.key.name.clone(), m.namespace.clone());
    ///     }
    ///     bundle
    /// }
    /// ```
    ///
    /// [`emit_module`]: Plugin::emit_module
    fn prepare_bundle(
        &self,
        _modules: &[Arc<CompiledModule>],
        _registry: &ModuleRegistry,
        _ctx: &ExpansionCtx<'_>,
    ) -> BundleState {
        BundleState::new()
    }

    /// Emit output for all user modules in display order.
    ///
    /// The default implementation calls [`prepare_bundle`] once, then
    /// [`emit_module`] for each module (separated by a blank line) with a
    /// fresh [`EmitState`] per module.
    ///
    /// [`prepare_bundle`]: Plugin::prepare_bundle
    fn emit(
        &self,
        modules: &[Arc<CompiledModule>],
        registry: &ModuleRegistry,
        ctx: &ExpansionCtx<'_>,
        out: &mut dyn Write,
    ) -> std::io::Result<()> {
        let bundle = self.prepare_bundle(modules, registry, ctx);
        let mut first = true;
        for module in modules {
            if !first {
                writeln!(out)?;
            }
            first = false;
            self.emit_module(module, registry, ctx, &bundle, &mut EmitState::new(), out)?;
        }
        Ok(())
    }

    /// Emit output for a single module.
    ///
    /// Per-module plugins override this; all-at-once plugins override
    /// [`emit`] instead and leave this as a no-op.
    ///
    /// `bundle` is shared (read-only) state produced by [`prepare_bundle`]
    /// before the per-module loop.  Use [`BundleState::get`] to retrieve
    /// typed data pre-computed for the whole bundle.
    ///
    /// `state` is a fresh [`EmitState`] created by the framework for this
    /// module.  Use [`EmitState::get_or_insert`] to store typed mutable state
    /// without needing `&mut self` or interior mutability on the plugin struct.
    fn emit_module(
        &self,
        _module: &Arc<CompiledModule>,
        _registry: &ModuleRegistry,
        _ctx: &ExpansionCtx<'_>,
        _bundle: &BundleState,
        _state: &mut EmitState,
        _out: &mut dyn Write,
    ) -> std::io::Result<()> {
        Ok(())
    }

    /// Called once after all modules have been emitted.
    /// Default implementation is a no-op.
    fn finish_bundle(&self, _bundle: &BundleState) {}
}
