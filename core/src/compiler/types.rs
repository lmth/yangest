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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhenExpr {
    pub xpath: String,
    pub description: Option<String>,
    pub reference: Option<String>,
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
}

pub struct SchemaNode {
    pub name: String,
    pub module_name: String,
    /// The own prefix of the module that defines this node (e.g. `"oc-sr-rsvp-ext"`).
    /// Used when rendering augmented nodes in a foreign module's tree where the
    /// augmenting module is not in the target module's prefix_map.
    pub module_prefix: String,
    pub pos: Pos,
    pub status: Status,
    pub config: Option<bool>,
    pub when: Vec<WhenExpr>,
    pub if_features: Vec<IfFeatureExpr>,
    pub description: Option<String>,
    pub reference: Option<String>,
    /// Extension statements applied to this node, in declaration order.
    pub extensions: Vec<ExtensionInstance>,
    pub kind: SchemaNodeKind,
    pub pmap: PMap,
}

impl std::fmt::Debug for SchemaNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchemaNode")
            .field("name", &self.name)
            .field("module_name", &self.module_name)
            .field("module_prefix", &self.module_prefix)
            .field("pos", &self.pos)
            .field("status", &self.status)
            .field("config", &self.config)
            .field("when", &self.when)
            .field("if_features", &self.if_features)
            .field("description", &self.description)
            .field("reference", &self.reference)
            .field("extensions", &self.extensions)
            .field("kind", &self.kind)
            .finish()
    }
}

impl Clone for SchemaNode {
    fn clone(&self) -> Self {
        SchemaNode {
            name: self.name.clone(),
            module_name: self.module_name.clone(),
            module_prefix: self.module_prefix.clone(),
            pos: self.pos.clone(),
            status: self.status,
            config: self.config,
            when: self.when.clone(),
            if_features: self.if_features.clone(),
            description: self.description.clone(),
            reference: self.reference.clone(),
            extensions: self.extensions.clone(),
            kind: self.kind.clone(),
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
}

#[derive(Debug, Clone)]
pub enum SchemaNodeKind {
    Container {
        presence: Option<String>,
        children: Vec<SchemaNode>,
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
        children: Vec<SchemaNode>,
        min_elements: u64,
        max_elements: Option<u64>,
        ordered_by: OrderedBy,
        musts: Vec<MustExpr>,
    },
    Choice {
        default: Option<String>,
        mandatory: bool,
        cases: Vec<SchemaNode>,
    },
    Case {
        children: Vec<SchemaNode>,
    },
    Rpc {
        input: Vec<SchemaNode>,
        output: Vec<SchemaNode>,
        musts: Vec<MustExpr>,
    },
    Action {
        input: Vec<SchemaNode>,
        output: Vec<SchemaNode>,
    },
    Notification {
        children: Vec<SchemaNode>,
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
        overlay: UsesOverlay,
    },
}

pub struct AugmentEntry {
    pub target_path: SchemaPath,
    pub nodes: Vec<SchemaNode>,
    pub when: Vec<WhenExpr>,
    pub if_features: Vec<IfFeatureExpr>,
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
    /// Name of the plugin that registered the corresponding [`OverlayExtension`].
    pub source_plugin: &'static str,
}

pub type NodeOverlayMap = HashMap<SchemaPath, NodeOverlay>;

pub type PrefixMap = indexmap::IndexMap<String, String>;

/// The deviation modules that were applied to this module during compilation.
///
/// Stored via [`CompiledModule::set_pdata`] during compilation.
/// Each entry is `(module_name, revision)` — revision is `None` when unspecified.
///
/// Retrieved by plugins with `module.pdata::<AppliedDeviations>()`.
pub struct AppliedDeviations(pub Vec<(String, Option<String>)>);

/// The annotation modules that were applied to this module during compilation.
///
/// Stored via [`CompiledModule::set_pdata`] during compilation.
/// Each entry is `(module_name, revision, prefix_map)` where `prefix_map` maps
/// prefix → module_name for all imports declared in the annotation module.
///
/// Retrieved by plugins with `module.pdata::<AppliedAnnotations>()`.
pub struct AppliedAnnotations(pub Vec<(String, Option<String>, PrefixMap)>);

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
        self.name_index.insert(module.key.name.clone(), Arc::clone(&module));
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
