// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::{ModuleKey, Pos, Stmt, YError};
use crate::grammar::GrammarRegistry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YangVersion {
    V1,
    V11,
}

pub type SchemaPath = Vec<PathStep>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PathStep {
    pub prefix: Option<String>,
    pub name: String,
}

pub type PMap = HashMap<TypeId, Box<dyn Any + Send + Sync>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Current,
    Deprecated,
    Obsolete,
}

/// Flags that affect compilation behavior. Set once before compilation starts.
#[derive(Debug, Clone, Default)]
pub struct CompilationFlags {
    /// Suppress errors for unknown prefixes/features in if-feature expressions.
    /// Unknown features are treated as disabled (false) at expansion time.
    pub ignore_unknown_features: bool,
    /// Extension on a typedef that provides an informational description string.
    /// When present, its argument is stored in [`Typedef::ext_info`].
    /// Format: `(module_name, extension_name)`.
    pub typedef_info_extension: Option<(String, String)>,
    /// Extension on a typedef that marks the type as opaque (resolved externally
    /// at runtime rather than from the YANG type hierarchy).
    /// Format: `(module_name, extension_name)`.
    /// When present, its argument is stored in [`Typedef::opaque_type_name`].
    pub opaque_type_extension: Option<(String, String)>,
    /// Extension that declares an explicit dependency path inside a `must` or
    /// `when` statement. When set, the argument of each occurrence is collected
    /// into [`MustExpr::explicit_deps`] / [`WhenExpr::explicit_deps`].
    /// Format: `(module_name, extension_name)`.
    pub dependency_extension: Option<(String, String)>,
    /// Extension that suppresses XPath-derived auto-dependency analysis inside
    /// a `must` or `when` statement. When present on the statement, sets
    /// [`MustExpr::override_auto_deps`] / [`WhenExpr::override_auto_deps`] to
    /// `true`. Format: `(module_name, extension_name)`.
    pub override_auto_deps_extension: Option<(String, String)>,
    /// Scope annotation-injected statements to the module the annotation module
    /// targets. When `true`, a `must`/`when` or extension (e.g. a `hidden` /
    /// `cli-*` flag) injected into a `grouping` body by an `annotate-module "X"`
    /// overlay is dropped from the expanded tree of any module *other than* X that
    /// reuses the grouping via `uses` — matching reference (yanger) behaviour,
    /// where such statements do not cross module boundaries. Off by default: the
    /// standard YANG reading is
    /// that annotating a grouping modifies the grouping for every consumer.
    /// Opt-in because it (a) changes output for all plugins and (b) makes a
    /// grouping's expansion consumer-dependent, defeating Arc-sharing for the
    /// affected groupings. Requires the [`AstAnnotationIndex`] on the
    /// [`ExpansionCtx`](crate::compiler::ExpansionCtx).
    pub scope_grouping_annotations_to_target: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IfFeatureExpr {
    Name(String, Option<String>),
    Not(Box<IfFeatureExpr>),
    And(Box<IfFeatureExpr>, Box<IfFeatureExpr>),
    Or(Box<IfFeatureExpr>, Box<IfFeatureExpr>),
}

#[derive(Debug, Clone)]
pub struct Feature {
    pub name: String,
    pub if_features: Vec<IfFeatureExpr>,
    pub status: Status,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Identity {
    pub name: String,
    pub bases: Vec<(Option<String>, String)>,
    pub if_features: Vec<IfFeatureExpr>,
    pub status: Status,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Typedef {
    pub name: String,
    pub type_stmt: Stmt,
    pub units: Option<String>,
    pub default: Option<String>,
    pub status: Status,
    pub description: Option<String>,
    /// Informational text from a plugin-registered extension (e.g. an "info"
    /// extension providing a short human-readable description of the type).
    /// Populated during compilation when [`CompilationFlags::typedef_info_extension`]
    /// is configured and the matching extension is found on the typedef.
    pub ext_info: Option<String>,
    /// True when an extension marks this typedef as having an externally-resolved
    /// (opaque) type that is not part of the standard YANG type hierarchy.
    pub has_opaque_type: bool,
    /// The argument of the opaque-type extension, if present (e.g. a named
    /// type-point identifier used to resolve the type at runtime).
    pub opaque_type_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Grouping {
    pub name: String,
    pub status: Status,
    pub description: Option<String>,
    pub stmt: Stmt,
    /// The prefix map of the module where this grouping was defined.
    /// Used to resolve prefixes inside the grouping body at expansion time.
    pub def_prefix_map: PrefixMap,
    /// The module's own prefix where this grouping was defined.
    pub def_own_prefix: String,
    /// The module name where this grouping was defined.
    pub definer_module_name: String,
    /// Groupings available at the scope where this grouping was defined.
    /// Populated for nested groupings (defined inside another grouping or schema node)
    /// so that sibling groupings are available during fallback expansion.
    pub scope_groupings: Option<Arc<IndexMap<String, Grouping>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedBy {
    System,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MustExpr {
    pub xpath: String,
    pub error_message: Option<String>,
    pub error_app_tag: Option<String>,
    pub description: Option<String>,
    pub source_module: String,
    /// Revision of the source module. `None` for must statements in the target module itself.
    /// Set when the must was injected by an annotation module.
    pub source_revision: Option<String>,
    /// Explicit dependency paths from sub-statements of the configured
    /// dependency extension (see [`CompilationFlags::dependency_extension`]).
    pub explicit_deps: Vec<String>,
    /// True when the configured override-auto-dependencies extension is
    /// present (see [`CompilationFlags::override_auto_deps_extension`]):
    /// consumers should use only `explicit_deps` and skip XPath dep analysis.
    pub override_auto_deps: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhenExpr {
    pub xpath: String,
    pub description: Option<String>,
    pub reference: Option<String>,
    pub source_module: String,
    /// Revision of the source module. `None` for regular when statements (uses the
    /// owning node's module revision). Set for annotation-originated when conditions.
    pub source_revision: Option<String>,
    /// True when this when expression was inherited from a `uses` or `augment` statement
    /// (non-local origin). When true, the expression is evaluated from the parent
    /// context, so dependency analysis must prepend a parent step.
    pub non_local: bool,
    /// Explicit dependency paths from sub-statements of the configured
    /// dependency extension (see [`CompilationFlags::dependency_extension`]).
    pub explicit_deps: Vec<String>,
    /// True when the configured override-auto-dependencies extension is
    /// present (see [`CompilationFlags::override_auto_deps_extension`]):
    /// consumers should use only `explicit_deps` and skip XPath dep analysis.
    pub override_auto_deps: bool,
}

/// An extension statement applied to a schema node or module during compilation.
#[derive(Debug, Clone)]
pub struct ExtensionInstance {
    /// The resolved module name of the extension (e.g. `"acme-ext"`).
    pub module: String,
    /// The local extension name (e.g. `"callpoint"`).
    pub name: String,
    /// The extension's argument, if any.
    pub arg: Option<String>,
    /// Raw sub-statements.  The plugin that declared this extension's grammar
    /// can interpret them according to its [`ExtensionGrammar::substmts`] rules.
    pub substmts: Vec<Stmt>,
    /// Source position of the extension statement (used for ordering, e.g.
    /// extension entries may need to be sorted by source position).
    pub pos: Pos,
    /// When this instance was *injected* onto a node by an annotation module
    /// (rather than written in the node's own source), the name of that
    /// annotation module. `None` for extensions written directly in the source.
    ///
    /// This distinguishes the extension's *definition* module ([`module`](Self::module),
    /// e.g. the module declaring `callpoint`) from the *injection source* module
    /// (the `*-ann` overlay that attached it). Backends that build a
    /// namespace-encounter map need the latter; see
    /// [`source_for_ns`](Self::source_for_ns).
    pub injection_source_module: Option<String>,
}

impl ExtensionInstance {
    /// The module to attribute this extension to for namespace/encounter-order
    /// purposes: the injection-source module if it was annotation-injected,
    /// otherwise its definition module.
    pub fn source_for_ns(&self) -> &str {
        self.injection_source_module.as_deref().unwrap_or(&self.module)
    }
}

pub struct SchemaNode {
    pub name: String,
    pub module_name: String,
    /// The own prefix of the module that defines this node (e.g. `"oc-sr-rsvp-ext"`).
    /// Used when rendering augmented nodes in a foreign module's tree where the
    /// augmenting module is not in the target module's prefix_map.
    pub module_prefix: String,
    /// The module that DEFINED this node — equals the grouping definer for nodes
    /// expanded from a grouping, the augmenting module for augmented nodes, and
    /// the module itself for locally-defined nodes.
    pub origin_module: String,
    pub pos: Pos,
    pub status: Status,
    /// Source position of the `status` substatement, when one was explicitly
    /// declared on this node. `None` when `status` is implicit (`current`), or
    /// when the node is synthesised (e.g. an implicit `case`). Lets a backend
    /// order `status` against other substatements (extensions, etc.) by source
    /// position when reproducing declaration-order-sensitive output.
    pub status_pos: Option<Pos>,
    pub config: Option<bool>,
    pub when: Vec<WhenExpr>,
    pub if_features: Vec<IfFeatureExpr>,
    pub description: Option<String>,
    pub reference: Option<String>,
    /// Extension statements applied to this node, in declaration order.
    pub extensions: Vec<ExtensionInstance>,
    pub kind: SchemaNodeKind,
    /// True when this node was grafted into the host tree by a remote
    /// (cross-module) `augment`, as opposed to a `uses` expansion or the
    /// module's own data tree. `module_name`/`origin_module` alone cannot tell
    /// these apart — a cross-module grouping use also leaves `module_name` as a
    /// foreign module. Backends use this to reproduce the reference compiler's
    /// per-step module qualification of leafref paths (yanger's
    /// `set_module_name_and_config`, called only from `apply_remote_augments`):
    /// augment-grafted steps are qualified `{module, name}`, while uses-expanded
    /// and own-tree steps stay bare.
    ///
    /// Set at compile time on the stored augment body (`AugmentEntry::nodes`) so
    /// every materialisation path inherits it through the node clone. A `uses`
    /// nested *inside* an augment body leaves its expanded children unmarked (the
    /// expansion is deferred past compile); the reference rewrites those too, but
    /// no observed case needs it.
    pub is_augment_injected: bool,
    pub pmap: PMap,
}

impl std::fmt::Debug for SchemaNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchemaNode")
            .field("name", &self.name)
            .field("module_name", &self.module_name)
            .field("module_prefix", &self.module_prefix)
            .field("origin_module", &self.origin_module)
            .field("pos", &self.pos)
            .field("status", &self.status)
            .field("config", &self.config)
            .field("when", &self.when)
            .field("if_features", &self.if_features)
            .field("description", &self.description)
            .field("reference", &self.reference)
            .field("extensions", &self.extensions)
            .field("kind", &self.kind)
            .field("is_augment_injected", &self.is_augment_injected)
            .finish()
    }
}

impl Clone for SchemaNode {
    fn clone(&self) -> Self {
        SchemaNode {
            name: self.name.clone(),
            module_name: self.module_name.clone(),
            module_prefix: self.module_prefix.clone(),
            origin_module: self.origin_module.clone(),
            pos: self.pos.clone(),
            status: self.status,
            status_pos: self.status_pos.clone(),
            config: self.config,
            when: self.when.clone(),
            if_features: self.if_features.clone(),
            description: self.description.clone(),
            reference: self.reference.clone(),
            extensions: self.extensions.clone(),
            kind: self.kind.clone(),
            is_augment_injected: self.is_augment_injected,
            pmap: HashMap::new(), // pmap holds renderer-private data; not cloned
        }
    }
}

impl SchemaNode {
    /// All extension instances attached to this node, in declaration order.
    pub fn extensions(&self) -> &[ExtensionInstance] {
        &self.extensions
    }

    /// Look up a specific extension by its module and local name.
    ///
    /// Returns the first matching instance, or `None` if not present.
    pub fn extension(&self, module: &str, name: &str) -> Option<&ExtensionInstance> {
        self.extensions
            .iter()
            .find(|e| e.module == module && e.name == name)
    }

    /// All `must` expressions on this node, in declaration order. Returns an
    /// empty slice for kinds that cannot carry `must` (e.g. `Choice`, `Case`,
    /// `Action`, `Uses`).
    pub fn musts(&self) -> &[MustExpr] {
        match &self.kind {
            SchemaNodeKind::Container { musts, .. }
            | SchemaNodeKind::Leaf { musts, .. }
            | SchemaNodeKind::LeafList { musts, .. }
            | SchemaNodeKind::List { musts, .. }
            | SchemaNodeKind::Rpc { musts, .. }
            | SchemaNodeKind::Notification { musts, .. }
            | SchemaNodeKind::AnyXml { musts, .. }
            | SchemaNodeKind::AnyData { musts, .. } => musts,
            SchemaNodeKind::Choice { .. }
            | SchemaNodeKind::Case { .. }
            | SchemaNodeKind::Action { .. }
            | SchemaNodeKind::Uses { .. } => &[],
        }
    }
}

#[derive(Debug, Clone)]
pub enum SchemaNodeKind {
    Container {
        presence: Option<String>,
        children: Arc<Vec<SchemaNode>>,
        musts: Vec<MustExpr>,
    },
    Leaf {
        type_stmt: Stmt,
        units: Option<String>,
        default: Option<String>,
        mandatory: bool,
        musts: Vec<MustExpr>,
    },
    LeafList {
        type_stmt: Stmt,
        units: Option<String>,
        default: Vec<String>,
        min_elements: u64,
        max_elements: Option<u64>,
        ordered_by: OrderedBy,
        musts: Vec<MustExpr>,
    },
    List {
        key: Vec<String>,
        unique: Vec<String>,
        children: Arc<Vec<SchemaNode>>,
        min_elements: u64,
        max_elements: Option<u64>,
        ordered_by: OrderedBy,
        musts: Vec<MustExpr>,
    },
    Choice {
        default: Option<String>,
        mandatory: bool,
        cases: Arc<Vec<SchemaNode>>,
    },
    Case {
        children: Arc<Vec<SchemaNode>>,
    },
    Rpc {
        input: Arc<Vec<SchemaNode>>,
        output: Arc<Vec<SchemaNode>>,
        musts: Vec<MustExpr>,
    },
    Action {
        input: Arc<Vec<SchemaNode>>,
        output: Arc<Vec<SchemaNode>>,
    },
    Notification {
        children: Arc<Vec<SchemaNode>>,
        musts: Vec<MustExpr>,
    },
    AnyXml {
        mandatory: bool,
        musts: Vec<MustExpr>,
    },
    AnyData {
        mandatory: bool,
        musts: Vec<MustExpr>,
    },
    /// Lazy grouping instantiation. Plugins never see this variant directly;
    /// they call `children(ctx)` which expands it transparently.
    Uses {
        grouping: Arc<Grouping>,
        /// Module name of the grouping source (for cross-module lookups at expansion time).
        source_module_name: Option<String>,
        /// True if the `uses` statement had NO prefix (e.g., `uses foo`).
        /// False if it had any prefix (e.g., `uses mymod:foo` or `uses ext:foo`).
        /// Emit plugins may use this to control whether to recurse into the grouping
        /// for same-module vs cross-module grouping references.
        was_unprefixed: bool,
        overlay: UsesOverlay,
    },
}

pub struct AugmentEntry {
    pub target_path: SchemaPath,
    pub nodes: Vec<SchemaNode>,
    pub when: Vec<WhenExpr>,
    pub if_features: Vec<IfFeatureExpr>,
    /// Status from the `augment` statement (RFC 6020 §7.15).
    /// All nodes added by this augment have their status restricted to this value.
    pub status: Status,
    /// Source position of the `augment` statement, carried so backends can apply
    /// augments in source order. The reference compiler orders CS records and
    /// hash-dictionary insertions by source position, which differs from
    /// "module name then keyword". Use [`Pos::orig_file`]/[`Pos::orig_line`] to
    /// order by definition site for augments injected through a grouping.
    pub pos: Pos,
}

/// Overlay carried by a `uses` node: use-site refinements, local augments,
/// and inherited when/if-feature constraints.
#[derive(Debug, Clone)]
pub struct UsesOverlay {
    /// Raw `refine` sub-statements from the `uses` body.
    pub refine_stmts: Vec<Stmt>,
    /// Pre-compiled local augment targets from `augment` sub-statements inside the `uses`.
    pub local_augments: Vec<LocalAugmentEntry>,
    /// Inherited `when` expressions (from the `uses` statement itself).
    pub when: Vec<WhenExpr>,
    /// Inherited `if-feature` expressions.
    pub if_features: Vec<IfFeatureExpr>,
}

impl UsesOverlay {
    pub fn is_empty(&self) -> bool {
        self.refine_stmts.is_empty()
            && self.local_augments.is_empty()
            && self.when.is_empty()
            && self.if_features.is_empty()
    }
}

/// A pre-compiled local augment from a `uses` body.
#[derive(Debug, Clone)]
pub struct LocalAugmentEntry {
    /// Relative path within the expansion (list of unqualified node names).
    pub target_path: Vec<String>,
    pub nodes: Vec<SchemaNode>,
    pub when: Vec<WhenExpr>,
    pub if_features: Vec<IfFeatureExpr>,
}

/// Per-node overlay: deviations + plugin annotations to be applied at expansion time.
#[derive(Debug, Clone, Default)]
pub struct NodeOverlay {
    /// Raw `deviate` sub-statements (each Stmt has arg = "add"/"replace"/"delete"/"not-supported").
    pub deviate_stmts: Vec<Stmt>,
    pub annotations: Vec<Annotation>,
    pub source_module: Option<String>,
}

/// A plugin annotation attached to a schema node by path.
///
/// Produced by the ANNINDEX phase from overlay modules. At expansion time,
/// `instances` are merged into the target node's `extensions` list so that
/// plugins can read them via the standard `node.extensions` API.
#[derive(Debug, Clone)]
pub struct Annotation {
    /// Resolved extension instances from the annotation body (sub-statements of
    /// the `annotate`-style extension statement in the overlay module).
    pub instances: Vec<ExtensionInstance>,
    /// Raw `when` statements from the annotation body; applied to the target
    /// node's `when` list with the annotation module as source.
    pub when_stmts: Vec<Stmt>,
    /// Raw `must` statements from the annotation body; applied to the target
    /// node's `must` list with the annotation module as source.
    pub must_stmts: Vec<Stmt>,
    /// Name of the annotation module that declared this annotation.
    pub source_module: String,
    /// Revision of the annotation module that declared this annotation.
    pub source_revision: Option<String>,
    /// Name of the plugin that registered the corresponding [`OverlayExtension`].
    pub source_plugin: &'static str,
}

/// Overlay map key: a name-only path plus an optional *leaf module*
/// qualifier (the module that owns the target leaf node).
///
/// The name-only path lets nodes from expanded groupings (which may retain a
/// different module prefix than the one used in the deviation/annotation path)
/// still match. The optional leaf-module disambiguates same-named siblings that
/// come from different modules — e.g. two augments contributing a `interface`
/// node under the same target — which would otherwise collide on the name path
/// alone and apply the wrong (or both) annotations.
///
/// Every entry is stored under both a qualified key `(path, Some(module))` and
/// an unqualified key `(path, None)`. Lookups try the qualified key first (using
/// the node's own module) and fall back to the unqualified key, so the precise
/// match wins where modules are known while the legacy name-only behaviour is
/// preserved wherever they are not. See [`crate::compiler::overlay_name_path`].
pub type OverlayKey = (Vec<String>, Option<String>);
pub type NodeOverlayMap = HashMap<OverlayKey, NodeOverlay>;

pub type PrefixMap = indexmap::IndexMap<String, String>;

/// The deviation modules that were applied to this module during compilation.
///
/// Stored via [`CompiledModule::set_pdata`] during compilation.
/// Each entry is `(module_name, revision, prefix_map, has_must_or_when)` where
/// `prefix_map` maps prefix → module_name for all imports declared in the
/// deviation module, and `has_must_or_when` is true when the deviation adds
/// or replaces `must` or `when` statements.
///
/// Retrieved by plugins with `module.pdata::<AppliedDeviations>()`.
pub struct AppliedDeviations(pub Vec<(String, Option<String>, PrefixMap, bool)>);

/// The annotation modules that were applied to this module during compilation.
///
/// Stored via [`CompiledModule::set_pdata`] during compilation.
/// Each entry is `(module_name, revision, prefix_map, has_when_or_must)` where
/// `prefix_map` maps prefix → module_name for all imports declared in the
/// annotation module, and `has_when_or_must` is true when the annotation adds
/// `when` or `must` statements to any target node.
///
/// Retrieved by plugins with `module.pdata::<AppliedAnnotations>()`.
///
/// Each entry: `(name, rev, prefix_map, has_wm, root_is_self, ext_prefixes)` where:
/// - `root_is_self` is `true` if the annotation module's annotate paths start with
///   a prefix that resolves to the target (base) module itself — meaning the annotation
///   targets the base module's own nodes directly, so the annotation module's namespace
///   should appear in the base module's namespace-to-prefix map entry.
/// - `ext_prefixes` is the sorted, deduplicated list of YANG prefixes found in extension
///   instance arguments (e.g. dependency-path arguments like `/a:root/b:sub:...`).
///   These prefixes must be added to the target module's yang_header imports even when there
///   are no when/must expressions (`has_wm` is false).
pub struct AppliedAnnotations(pub Vec<(String, Option<String>, PrefixMap, bool, bool, Vec<String>)>);

/// AST annotation modules applied via annotate-module + annotate-statement overlay extensions.
///
/// These annotations are merged into the target module's AST *before* compilation, so they
/// do not appear in [`AppliedAnnotations`] (which only covers path-based node annotations).
/// Plugins use this to discover which annotation modules contributed extension arguments
/// that reference external prefixes.
///
/// Each entry: `(ann_module_name, ann_module_revision, prefix_map)`.
///
/// Stored via [`CompiledModule::set_pdata`] after compilation in the main driver.
/// Retrieved by plugins with `module.pdata::<AstAppliedAnnotations>()`.
pub struct AstAppliedAnnotations(pub Vec<(String, Option<String>, std::collections::HashMap<String, String>)>);

pub struct CompiledModule {
    pub key: ModuleKey,
    pub yang_version: YangVersion,
    pub namespace: String,
    pub prefix: String,
    pub prefix_map: PrefixMap,
    pub typedefs: IndexMap<String, Typedef>,
    pub groupings: Arc<IndexMap<String, Grouping>>,
    pub features: IndexMap<String, Feature>,
    pub identities: IndexMap<String, Identity>,
    pub children: Vec<SchemaNode>,
    pub augments: Vec<AugmentEntry>,
    pub overlay: NodeOverlayMap,
    pub errors: Vec<YError>,
    pub stmt: Stmt,
    pub pmap: PMap,
    /// Extension statements declared at the module level (direct sub-statements of `module`).
    pub extensions: Vec<ExtensionInstance>,
    /// Import module names declared in this module (in declaration order),
    /// regardless of whether they were resolved. Used by the depend plugin.
    pub imports: Vec<String>,
    /// Submodule names referenced by 'include' statements (in declaration order).
    pub includes: Vec<String>,
    /// The filesystem path from which this module was loaded.
    pub source_path: Option<PathBuf>,
    /// Pre-compiled (but not yet fully-expanded) children for each grouping body,
    /// indexed by grouping name.  Used by `expand_uses_lazy` to skip re-running
    /// `compile_schema_children` on every plugin invocation.
    pub grouping_children: HashMap<String, Arc<Vec<SchemaNode>>>,
}

impl CompiledModule {
    /// Store compile-phase data keyed by type `T`.
    ///
    /// Intended for use inside `compile_module` (or any post-compile step) to attach
    /// typed data that plugins can retrieve later via [`pdata`](Self::pdata).
    /// Overwrites any previously stored value of the same type.
    pub fn set_pdata<T: Any + Send + Sync>(&mut self, val: T) {
        self.pmap.insert(TypeId::of::<T>(), Box::new(val));
    }

    /// Retrieve compile-phase data of type `T` previously stored with [`set_pdata`](Self::set_pdata).
    ///
    /// Returns `None` if no value of that type has been stored.
    pub fn pdata<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.pmap.get(&TypeId::of::<T>())?.downcast_ref()
    }
}

pub struct ModuleRegistry {
    pub modules: IndexMap<ModuleKey, Arc<CompiledModule>>,
    /// Fast name-only lookup: module name → most-recently-inserted revision.
    /// Populated by [`insert`](Self::insert); used by [`resolve_import`](Self::resolve_import)
    /// for the common revision-less case.
    name_index: HashMap<String, Arc<CompiledModule>>,
    /// Extension grammar rules registered by plugins before compilation.
    pub grammar: GrammarRegistry,
    pub flags: CompilationFlags,
}

impl Default for ModuleRegistry {
    fn default() -> Self {
        Self {
            modules: IndexMap::new(),
            name_index: HashMap::new(),
            grammar: GrammarRegistry::new(),
            flags: CompilationFlags::default(),
        }
    }
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, module: Arc<CompiledModule>) {
        // The name-only index must point at the *latest* revision so that a
        // revision-less `resolve_import` / `include` resolves to it (RFC 7950
        // §5.1.1), regardless of compile/insert order.
        let is_latest = match self.name_index.get(&module.key.name) {
            Some(existing) => {
                crate::ast::ModuleKey::revision_cmp(
                    module.key.revision.as_deref(),
                    existing.key.revision.as_deref(),
                ) != std::cmp::Ordering::Less
            }
            None => true,
        };
        if is_latest {
            self.name_index.insert(module.key.name.clone(), Arc::clone(&module));
        }
        self.modules.insert(module.key.clone(), module);
    }

    pub fn get(&self, key: &ModuleKey) -> Option<Arc<CompiledModule>> {
        self.modules.get(key).cloned()
    }

    pub fn resolve_import(
        &self,
        name: &str,
        revision: Option<&str>,
    ) -> Option<Arc<CompiledModule>> {
        if let Some(revision) = revision {
            let key = ModuleKey {
                name: name.to_string(),
                revision: Some(revision.to_string()),
            };
            if let Some(module) = self.modules.get(&key) {
                return Some(Arc::clone(module));
            }
        }

        // Fast path: O(1) name-only lookup via index.
        if let Some(module) = self.name_index.get(name) {
            return Some(Arc::clone(module));
        }

        None
    }

    /// Returns true if any module in the registry has a non-empty overlay
    /// (i.e. deviations targeting nodes inside `uses` expansions).
    /// Used by `ExpansionCtx` to skip path-tracking when there is nothing to look up.
    pub fn has_any_overlay(&self) -> bool {
        self.modules.values().any(|m| !m.overlay.is_empty())
    }
}
