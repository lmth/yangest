// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Global type registry: resolve any YANG `type` reference to a fully-resolved
//! [`ResolvedType`] (built-in base + typedef derivation chain + accumulated
//! restrictions + union members / leafref path / identity bases / enums / bits).
//!
//! This is an **opt-in** facility: it is constructed on demand by a backend from
//! an already-built [`ModuleRegistry`] and is never touched by the compile path
//! or by `SchemaNode::children`, so plugins that don't resolve types (e.g. the
//! tree plugin) pay nothing.
//!
//! yangest-core itself stores types unresolved (a raw `type` [`Stmt`] on each
//! leaf/leaf-list and on each [`Typedef`]). Backends that emit a typed
//! representation — flattening typedef chains, leafrefs, and unions — use this
//! registry so the typedef-walking logic lives in one place instead of being
//! re-derived (and subtly diverging) in every backend.
//!
//! Leafref *target* resolution (walking the `path` to its target node and
//! resolving the target's type) lives in [`leafref`] and needs a
//! [`Cursor`](crate::cursor::Cursor); here a leafref's `path` is kept raw.

use std::cell::RefCell;
use std::collections::HashSet;
use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::{BuiltInKeyword, Stmt};
use crate::compiler::{ModuleRegistry, Typedef};

pub mod leafref;
mod observer;

pub use observer::{ConstraintKind, NoopObserver, TypeResolutionObserver};

// ── Built-in types (RFC 7950 §9) ─────────────────────────────────────────────

/// A YANG built-in (base) type. These are the 19 names that may appear as the
/// argument of a `type` statement without a prefix and that are not derived from
/// a typedef.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltInType {
    Binary,
    Bits,
    Boolean,
    Decimal64,
    Empty,
    Enumeration,
    IdentityRef,
    InstanceIdentifier,
    Int8,
    Int16,
    Int32,
    Int64,
    Leafref,
    String,
    Uint8,
    Uint16,
    Uint32,
    Uint64,
    Union,
}

impl BuiltInType {
    /// Map a YANG type name to a built-in type, or `None` if it is not a
    /// built-in (and therefore must be a typedef reference).
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "binary" => Self::Binary,
            "bits" => Self::Bits,
            "boolean" => Self::Boolean,
            "decimal64" => Self::Decimal64,
            "empty" => Self::Empty,
            "enumeration" => Self::Enumeration,
            "identityref" => Self::IdentityRef,
            "instance-identifier" => Self::InstanceIdentifier,
            "int8" => Self::Int8,
            "int16" => Self::Int16,
            "int32" => Self::Int32,
            "int64" => Self::Int64,
            "leafref" => Self::Leafref,
            "string" => Self::String,
            "uint8" => Self::Uint8,
            "uint16" => Self::Uint16,
            "uint32" => Self::Uint32,
            "uint64" => Self::Uint64,
            "union" => Self::Union,
            _ => return None,
        })
    }

    /// The canonical YANG name of this built-in type.
    pub fn name(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::Bits => "bits",
            Self::Boolean => "boolean",
            Self::Decimal64 => "decimal64",
            Self::Empty => "empty",
            Self::Enumeration => "enumeration",
            Self::IdentityRef => "identityref",
            Self::InstanceIdentifier => "instance-identifier",
            Self::Int8 => "int8",
            Self::Int16 => "int16",
            Self::Int32 => "int32",
            Self::Int64 => "int64",
            Self::Leafref => "leafref",
            Self::String => "string",
            Self::Uint8 => "uint8",
            Self::Uint16 => "uint16",
            Self::Uint32 => "uint32",
            Self::Uint64 => "uint64",
            Self::Union => "union",
        }
    }
}

// ── Resolved type description ─────────────────────────────────────────────────

/// One named value of an `enumeration` type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumValue {
    pub name: String,
    /// Explicit `value`, if declared.
    pub value: Option<i64>,
}

/// One named bit of a `bits` type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitValue {
    pub name: String,
    /// Explicit `position`, if declared.
    pub position: Option<u32>,
}

/// Value restrictions accumulated down a typedef derivation chain.
///
/// `range` and `length` hold the *most-derived* (effective, narrowest) value —
/// YANG requires each derived restriction to be a subset of the one it refines,
/// so the most-derived string is the binding constraint. `patterns` are
/// cumulative: every `pattern` along the chain applies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Restrictions {
    pub range: Option<String>,
    pub length: Option<String>,
    pub patterns: Vec<String>,
}

/// A fully-resolved type: the built-in base plus everything a backend needs to
/// emit a flat typed representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedType {
    /// The underlying built-in type the chain bottoms out in.
    pub base: BuiltInType,
    /// Typedef derivation chain, outermost (use-site) first, base built-in last
    /// (the built-in itself is not listed). Each entry is `(module, typedef)`.
    /// Empty for a direct built-in use.
    pub chain: Vec<(String, String)>,
    pub restrictions: Restrictions,
    /// For `union`: member types in declaration order.
    pub union_members: Vec<ResolvedType>,
    /// For `leafref`: the raw `path` argument (target resolution is in [`leafref`]).
    pub leafref_path: Option<String>,
    /// For `leafref`/`instance-identifier`: the `require-instance` value (default true).
    pub require_instance: bool,
    /// For `identityref`: the `(module, name)` of each `base` identity.
    pub identity_bases: Vec<(String, String)>,
    /// For `enumeration`: the declared values in order.
    pub enums: Vec<EnumValue>,
    /// For `bits`: the declared bits in order.
    pub bits: Vec<BitValue>,
    /// For `decimal64`: `fraction-digits`.
    pub fraction_digits: Option<u8>,
}

impl ResolvedType {
    fn of_builtin(base: BuiltInType) -> Self {
        ResolvedType {
            base,
            chain: Vec::new(),
            restrictions: Restrictions::default(),
            union_members: Vec::new(),
            leafref_path: None,
            require_instance: true,
            identity_bases: Vec::new(),
            enums: Vec::new(),
            bits: Vec::new(),
            fraction_digits: None,
        }
    }
}

// ── Errors ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeError {
    /// A `type` statement had no argument.
    MissingArgument,
    /// A prefix on a `type` argument did not resolve to any imported module.
    UnknownPrefix { ctx_module: String, prefix: String },
    /// The (resolved) defining module is not present in the registry.
    UnknownModule { module: String },
    /// No typedef of that name exists in the (resolved) defining module.
    UnknownTypedef { module: String, name: String },
    /// A typedef derivation cycle was detected.
    Cycle { module: String, name: String },
    /// A `type` statement was expected to be a leafref but was not.
    NotALeafref,
    /// A leafref `path` could not be parsed or used a construct (e.g. `deref()`)
    /// that target resolution does not yet support.
    UnsupportedLeafrefPath(String),
    /// A leafref `path` did not resolve to a target node.
    LeafrefTargetNotFound(String),
    /// A leafref's target node was not a leaf or leaf-list.
    LeafrefTargetNotLeaf { name: String },
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeError::MissingArgument => write!(f, "type statement is missing its argument"),
            TypeError::UnknownPrefix { ctx_module, prefix } => {
                write!(f, "unknown type prefix '{prefix}:' in module '{ctx_module}'")
            }
            TypeError::UnknownModule { module } => {
                write!(f, "module '{module}' not found in registry")
            }
            TypeError::UnknownTypedef { module, name } => {
                write!(f, "typedef '{name}' not found in module '{module}'")
            }
            TypeError::Cycle { module, name } => {
                write!(f, "typedef derivation cycle at '{module}:{name}'")
            }
            TypeError::NotALeafref => write!(f, "type is not a leafref"),
            TypeError::UnsupportedLeafrefPath(p) => {
                write!(f, "unsupported leafref path: {p}")
            }
            TypeError::LeafrefTargetNotFound(p) => {
                write!(f, "leafref target not found for path: {p}")
            }
            TypeError::LeafrefTargetNotLeaf { name } => {
                write!(f, "leafref target '{name}' is not a leaf or leaf-list")
            }
        }
    }
}

impl std::error::Error for TypeError {}

// ── The registry ─────────────────────────────────────────────────────────────

/// Resolves type references against an already-built [`ModuleRegistry`].
///
/// Construct one per backend invocation. Resolution results for module-qualified
/// typedefs are memoized on the instance; the cache never touches the registry
/// or any [`CompiledModule`], so it is safe to drop at any time.
pub struct TypeRegistry<'a> {
    registry: &'a ModuleRegistry,
    /// Memo of fully-resolved typedefs, keyed by `(defining_module, typedef)`.
    /// Holds the resolution *without* use-site restriction narrowing.
    cache: RefCell<HashMap<(String, String), Rc<ResolvedType>>>,
}

impl<'a> TypeRegistry<'a> {
    pub fn new(registry: &'a ModuleRegistry) -> Self {
        TypeRegistry {
            registry,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// The underlying registry (for backends that also need module lookups).
    pub fn registry(&self) -> &'a ModuleRegistry {
        self.registry
    }

    /// Resolve a built-in type to its (restriction-free) description.
    pub fn lookup_builtin(&self, base: BuiltInType) -> ResolvedType {
        ResolvedType::of_builtin(base)
    }

    /// Resolve a typedef by `(module, name)` to its fully-resolved type,
    /// *without* any use-site narrowing.
    pub fn lookup(&self, module: &str, name: &str) -> Option<ResolvedType> {
        let mut obs = NoopObserver;
        self.resolve_typedef(module, name, &mut HashSet::new(), &mut obs)
            .ok()
            .map(|rc| (*rc).clone())
    }

    /// Resolve a `type` statement appearing in `ctx_module` to a [`ResolvedType`],
    /// flattening the entire typedef chain and applying use-site restrictions.
    pub fn resolve(&self, type_stmt: &Stmt, ctx_module: &str) -> Result<ResolvedType, TypeError> {
        let mut obs = NoopObserver;
        self.resolve_with_observer(type_stmt, ctx_module, &mut obs)
    }

    /// Like [`resolve`](Self::resolve) but reports each resolved leafref/typedef
    /// to `observer` in encounter order (feature #7). The default `resolve` uses
    /// a [`NoopObserver`], which the compiler inlines away.
    pub fn resolve_with_observer(
        &self,
        type_stmt: &Stmt,
        ctx_module: &str,
        observer: &mut dyn TypeResolutionObserver,
    ) -> Result<ResolvedType, TypeError> {
        let mut visiting = HashSet::new();
        self.resolve_type(type_stmt, ctx_module, &mut visiting, observer)
    }

    // ── internals ────────────────────────────────────────────────────────────

    /// Resolve a `type` statement in the context of `ctx_module`.
    fn resolve_type(
        &self,
        type_stmt: &Stmt,
        ctx_module: &str,
        visiting: &mut HashSet<(String, String)>,
        observer: &mut dyn TypeResolutionObserver,
    ) -> Result<ResolvedType, TypeError> {
        let arg = type_stmt.arg.as_deref().ok_or(TypeError::MissingArgument)?;
        let (prefix, name) = split_prefixed(arg);

        match prefix {
            None => {
                if let Some(base) = BuiltInType::from_name(name) {
                    return Ok(self.build_builtin(base, type_stmt, ctx_module, visiting, observer));
                }
                // Unprefixed non-builtin: a typedef in the current module.
                let resolved = self.resolve_typedef(ctx_module, name, visiting, observer)?;
                Ok(self.narrow((*resolved).clone(), type_stmt))
            }
            Some(prefix) => {
                let def_module = self.module_for_prefix(ctx_module, prefix)?;
                let resolved = self.resolve_typedef(&def_module, name, visiting, observer)?;
                Ok(self.narrow((*resolved).clone(), type_stmt))
            }
        }
    }

    /// Resolve a typedef `(def_module, name)` to its cached, fully-resolved type
    /// (no use-site narrowing). Detects derivation cycles via `visiting`.
    fn resolve_typedef(
        &self,
        def_module: &str,
        name: &str,
        visiting: &mut HashSet<(String, String)>,
        observer: &mut dyn TypeResolutionObserver,
    ) -> Result<Rc<ResolvedType>, TypeError> {
        let key = (def_module.to_string(), name.to_string());

        // Cycle detection is on the active resolution path, not the cache.
        if visiting.contains(&key) {
            return Err(TypeError::Cycle {
                module: def_module.to_string(),
                name: name.to_string(),
            });
        }

        // Look up the typedef so the observer can be fired even on a cache hit
        // (encounter order matters for #7, so the observer call is outside the
        // memo). Resolution of the *value* is still memoized.
        let module = self
            .registry
            .resolve_import(def_module, None)
            .ok_or_else(|| TypeError::UnknownModule { module: def_module.to_string() })?;
        let typedef = module
            .typedefs
            .get(name)
            .ok_or_else(|| TypeError::UnknownTypedef {
                module: def_module.to_string(),
                name: name.to_string(),
            })?;
        observer.on_typedef_resolved(def_module, typedef);

        if let Some(cached) = self.cache.borrow().get(&key) {
            return Ok(Rc::clone(cached));
        }

        visiting.insert(key.clone());
        // The typedef body is resolved in the context of its *defining* module.
        let inner = self.resolve_type(&typedef.type_stmt, def_module, visiting, observer);
        visiting.remove(&key);
        let mut inner = inner?;

        // This typedef contributes a derivation-chain entry (outermost first).
        inner.chain.insert(0, key.clone());

        let rc = Rc::new(inner);
        self.cache.borrow_mut().insert(key, Rc::clone(&rc));
        Ok(rc)
    }

    /// Build a [`ResolvedType`] for a direct built-in use, parsing the
    /// restrictions / sub-structure carried by `type_stmt`.
    fn build_builtin(
        &self,
        base: BuiltInType,
        type_stmt: &Stmt,
        ctx_module: &str,
        visiting: &mut HashSet<(String, String)>,
        observer: &mut dyn TypeResolutionObserver,
    ) -> ResolvedType {
        let mut rt = ResolvedType::of_builtin(base);
        self.parse_restrictions(&mut rt.restrictions, type_stmt);

        match base {
            BuiltInType::Union => {
                for member in type_stmt.get_substmts(BuiltInKeyword::Type) {
                    // Malformed members degrade to skipped rather than aborting
                    // the whole resolution.
                    if let Ok(m) = self.resolve_type(member, ctx_module, visiting, observer) {
                        rt.union_members.push(m);
                    }
                }
            }
            BuiltInType::Leafref => {
                rt.leafref_path = type_stmt
                    .get_substmt(BuiltInKeyword::Path)
                    .and_then(|p| p.arg.clone());
                rt.require_instance = require_instance(type_stmt);
            }
            BuiltInType::InstanceIdentifier => {
                rt.require_instance = require_instance(type_stmt);
            }
            BuiltInType::IdentityRef => {
                for base_stmt in type_stmt.get_substmts(BuiltInKeyword::Base) {
                    if let Some(arg) = base_stmt.arg.as_deref() {
                        let (pfx, ident) = split_prefixed(arg);
                        let module = match pfx {
                            None => ctx_module.to_string(),
                            Some(p) => self
                                .module_for_prefix(ctx_module, p)
                                .unwrap_or_else(|_| ctx_module.to_string()),
                        };
                        rt.identity_bases.push((module, ident.to_string()));
                    }
                }
            }
            BuiltInType::Enumeration => {
                for e in type_stmt.get_substmts(BuiltInKeyword::EnumStmt) {
                    if let Some(name) = e.arg.clone() {
                        let value = e
                            .get_substmt(BuiltInKeyword::Value)
                            .and_then(|v| v.arg.as_deref())
                            .and_then(|s| s.trim().parse::<i64>().ok());
                        rt.enums.push(EnumValue { name, value });
                    }
                }
            }
            BuiltInType::Bits => {
                for b in type_stmt.get_substmts(BuiltInKeyword::Bit) {
                    if let Some(name) = b.arg.clone() {
                        let position = b
                            .get_substmt(BuiltInKeyword::Position)
                            .and_then(|p| p.arg.as_deref())
                            .and_then(|s| s.trim().parse::<u32>().ok());
                        rt.bits.push(BitValue { name, position });
                    }
                }
            }
            BuiltInType::Decimal64 => {
                rt.fraction_digits = type_stmt
                    .get_substmt(BuiltInKeyword::FractionDigits)
                    .and_then(|f| f.arg.as_deref())
                    .and_then(|s| s.trim().parse::<u8>().ok());
            }
            _ => {}
        }

        rt
    }

    /// Parse `range`/`length`/`pattern` restrictions directly on `type_stmt`.
    fn parse_restrictions(&self, restr: &mut Restrictions, type_stmt: &Stmt) {
        if let Some(r) = type_stmt
            .get_substmt(BuiltInKeyword::Range)
            .and_then(|s| s.arg.clone())
        {
            restr.range = Some(r);
        }
        if let Some(l) = type_stmt
            .get_substmt(BuiltInKeyword::Length)
            .and_then(|s| s.arg.clone())
        {
            restr.length = Some(l);
        }
        for p in type_stmt.get_substmts(BuiltInKeyword::Pattern) {
            if let Some(arg) = p.arg.clone() {
                restr.patterns.push(arg);
            }
        }
    }

    /// Apply use-site narrowing: a `range`/`length` on the using `type` statement
    /// overrides (narrows) the inherited one; patterns are additive.
    fn narrow(&self, mut resolved: ResolvedType, use_stmt: &Stmt) -> ResolvedType {
        if let Some(r) = use_stmt
            .get_substmt(BuiltInKeyword::Range)
            .and_then(|s| s.arg.clone())
        {
            resolved.restrictions.range = Some(r);
        }
        if let Some(l) = use_stmt
            .get_substmt(BuiltInKeyword::Length)
            .and_then(|s| s.arg.clone())
        {
            resolved.restrictions.length = Some(l);
        }
        for p in use_stmt.get_substmts(BuiltInKeyword::Pattern) {
            if let Some(arg) = p.arg.clone() {
                resolved.restrictions.patterns.push(arg);
            }
        }
        resolved
    }

    /// Resolve `prefix` (as it appears in `ctx_module`) to a defining module name.
    fn module_for_prefix(&self, ctx_module: &str, prefix: &str) -> Result<String, TypeError> {
        let module = self
            .registry
            .resolve_import(ctx_module, None)
            .ok_or_else(|| TypeError::UnknownModule { module: ctx_module.to_string() })?;
        // A prefix equal to the module's own prefix refers to the module itself;
        // `prefix_map` only carries import prefixes.
        if module.prefix == prefix {
            return Ok(ctx_module.to_string());
        }
        module
            .prefix_map
            .get(prefix)
            .cloned()
            .ok_or_else(|| TypeError::UnknownPrefix {
                ctx_module: ctx_module.to_string(),
                prefix: prefix.to_string(),
            })
    }
}

/// `prefix:name` → `(Some("prefix"), "name")`; `name` → `(None, "name")`.
pub(crate) fn split_prefixed(raw: &str) -> (Option<&str>, &str) {
    match raw.split_once(':') {
        Some((p, n)) => (Some(p), n),
        None => (None, raw),
    }
}

/// Read a `require-instance` sub-statement (default `true`).
fn require_instance(type_stmt: &Stmt) -> bool {
    type_stmt
        .get_substmt(BuiltInKeyword::RequireInstance)
        .and_then(|s| s.arg.as_deref())
        .map(|s| s.trim() != "false")
        .unwrap_or(true)
}

/// Convenience: resolve the `type` of a typedef's chain, for callers that hold a
/// [`Typedef`] directly.
pub fn resolve_typedef_type(
    registry: &TypeRegistry<'_>,
    typedef: &Typedef,
    def_module: &str,
) -> Result<ResolvedType, TypeError> {
    registry.resolve(&typedef.type_stmt, def_module)
}

#[cfg(test)]
mod tests;
