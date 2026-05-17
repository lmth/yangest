//! YANG built-in type → FXS type mapping.
//!
//! Maps `type_stmt.arg` strings to `(exs.type, exs.primitive_type, exs_flags)`
//! tuples, matching `yanger_tailf:builtin_type_map()` and
//! `yanger_fxs:mk_builtin_base()`.

use eetf::Term;

use crate::terms::{atom, tuple, undefined};

// Namespace constants (Erlang atoms in FXS)
const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema";
const CONFD_NS: &str = "http://tail-f.com/ns/confd/1.0";

/// F_EXS_IS_ENUMERATION from exs.hrl
pub const F_EXS_IS_ENUMERATION: u32 = 1 << 2;
/// F_EXS_IS_LEAF_LIST from exs.hrl
pub const F_EXS_IS_LEAF_LIST: u32 = 1 << 3;
/// F_EXS_OPTIONAL_NP_CONTAINER from exs.hrl
pub const F_EXS_OPTIONAL_NP_CONTAINER: u32 = 1 << 4;
/// F_EXS_READONLY from exs.hrl: set when config=false
pub const F_EXS_READONLY: u32 = 1 << 0;

/// Result of resolving a YANG type to its FXS representation.
pub struct TypeInfo {
    /// `exs.type` — `{Ns::atom(), TypeName::atom}` or `undefined`
    pub exs_type: Term,
    /// `exs.primitive_type` — atom like `string`, `integer`, `boolean`, or `undefined`
    pub primitive_type: Term,
    /// Extra bits to OR into `exs.flags`
    pub extra_exs_flags: u32,
    /// True when this type is a named typedef reference (not an inline builtin).
    pub is_typedef: bool,
    /// The module that defines this typedef (None for inline builtins).
    /// Used to determine if load_flags should be set: load_flags are needed when
    /// typedef_defining_module != file_module AND typedef_defining_module is not
    /// an IETF/builtin module (ietf-inet-types, ietf-yang-types, tailf-common, etc.).
    pub typedef_defining_module: Option<String>,
    /// True when this typedef ultimately resolves to an enumeration (even for cross-module
    /// typedefs where extra_exs_flags may be 0). Used for PARSE_DEFAULT vs GET_DEFAULT.
    pub is_enum_base: bool,
}

/// Map a YANG built-in type name to FXS type info.
///
/// Handles the standard YANG built-in types.  Derived types (typedef,
/// union, enumeration members) require additional processing not done here.
pub fn resolve_builtin_type(yang_type: &str) -> TypeInfo {
    match yang_type {
        "string" => xsd_type("string", "string"),
        "boolean" => xsd_type("boolean", "boolean"),
        "int8" => xsd_type("byte", "byte"),
        "int16" => xsd_type("short", "short"),
        "int32" => xsd_type("int", "int"),
        "int64" => xsd_type("long", "long"),
        "uint8" => xsd_type("unsignedByte", "unsignedByte"),
        "uint16" => xsd_type("unsignedShort", "unsignedShort"),
        "uint32" => xsd_type("unsignedInt", "unsignedInt"),
        "uint64" => xsd_type("unsignedLong", "unsignedLong"),
        "binary" => xsd_type("base64Binary", "base64Binary"),
        "decimal64" => confd_type("decimal64", "decimal64"),
        "empty" => confd_type("empty", "empty"),
        "instance-identifier" => confd_type("objectRef", "objectRef"),
        "identityref" => confd_type("identityref", "identityref"),
        "enumeration" => TypeInfo {
            exs_type: xsd_ns_pair("string"),
            primitive_type: atom("string"),
            extra_exs_flags: F_EXS_IS_ENUMERATION,
            is_typedef: false, typedef_defining_module: None,
            is_enum_base: true,
        },
        "bits" => TypeInfo {
            exs_type: confd_ns_pair("bits"),
            primitive_type: atom("bits_type_32"),
            extra_exs_flags: 0,
            is_typedef: false, typedef_defining_module: None,
            is_enum_base: false,
        },
        "union" => TypeInfo {
            exs_type: confd_ns_pair("union"),
            primitive_type: atom("union"),
            extra_exs_flags: 0,
            is_typedef: false, typedef_defining_module: None,
            is_enum_base: false,
        },
        "leafref" => TypeInfo {
            // leafref target type is determined at load time
            exs_type: xsd_ns_pair("string"),
            primitive_type: atom("string"),
            extra_exs_flags: 0,
            is_typedef: false, typedef_defining_module: None,
            is_enum_base: false,
        },
        _ => TypeInfo {
            // Unknown / typedef reference: fall back to string
            exs_type: xsd_ns_pair("string"),
            primitive_type: atom("string"),
            extra_exs_flags: 0,
            is_typedef: false, typedef_defining_module: None,
            is_enum_base: false,
        },
    }
}

fn xsd_type(xsd_name: &str, prim: &str) -> TypeInfo {
    TypeInfo {
        exs_type: xsd_ns_pair(xsd_name),
        primitive_type: atom(prim),
        extra_exs_flags: 0,
        is_typedef: false, typedef_defining_module: None,
        is_enum_base: false,
    }
}

fn confd_type(confd_name: &str, prim: &str) -> TypeInfo {
    TypeInfo {
        exs_type: confd_ns_pair(confd_name),
        primitive_type: atom(prim),
        extra_exs_flags: 0,
        is_typedef: false, typedef_defining_module: None,
        is_enum_base: false,
    }
}

fn xsd_ns_pair(name: &str) -> Term {
    ns_pair(XSD_NS, name)
}

fn confd_ns_pair(name: &str) -> Term {
    ns_pair(CONFD_NS, name)
}

fn ns_pair(ns: &str, name: &str) -> Term {
    tuple(vec![atom(ns), atom(name)])
}

/// Build `exs.type` and `exs.primitive_type` from a raw `type` statement arg.
///
/// Handles typedef references by stripping a `prefix:` qualifier and looking
/// up the local name as a built-in.  Returns `undefined` for both fields if
/// the type is not recognized (uncommon in practice for config nodes).
pub fn type_info_from_stmt_arg(type_arg: &str) -> TypeInfo {
    // Strip module prefix if present (e.g. "inet:ipv4-address" → "ipv4-address")
    let local = if let Some(pos) = type_arg.rfind(':') {
        &type_arg[pos + 1..]
    } else {
        type_arg
    };
    resolve_builtin_type(local)
}

/// Resolve a `type` statement arg with full registry context.
///
/// For prefixed typedef references (e.g. `"inet:ip-address"`):
///   1. Splits prefix and local name
///   2. Looks up the prefix in `module_prefix_map` to get the imported module name
///   3. Resolves the imported module to get its namespace
///   4. Returns `exs.type = {importing_module_ns, local_name}` with
///      `primitive_type = undefined` (overridden by IETF special cases)
///
/// For unqualified names that are not YANG built-ins, checks the current module's
/// typedefs and returns `{module_ns, typedef_name}` with `primitive_type = undefined`.
pub fn type_info_with_registry(
    type_arg: &str,
    node_module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
) -> TypeInfo {
    let (prefix, local) = if let Some(pos) = type_arg.find(':') {
        (&type_arg[..pos], &type_arg[pos + 1..])
    } else {
        // Unqualified name: try built-in first, then local typedef
        let builtin = type_info_from_stmt_arg(type_arg);
        // If the result is the fallback (xsd string), check local typedefs
        let is_fallback = matches!(&builtin.exs_type,
            eetf::Term::Tuple(elems) if elems.elements.len() == 2 &&
            matches!(&elems.elements[0], eetf::Term::Atom(a) if a.name == "http://www.w3.org/2001/XMLSchema") &&
            matches!(&elems.elements[1], eetf::Term::Atom(a) if a.name == "string")
        ) && builtin.extra_exs_flags == 0;
        if is_fallback {
            if let Some(module) = registry.resolve_import(node_module_name, None) {
                if let Some(td) = module.typedefs.get(type_arg) {
                    let td_base = td.type_stmt.arg.as_deref().unwrap_or("string");
                    let is_enum = is_enumeration_base(td_base, &module.key.name, registry);
                    let is_bits = !is_enum && is_bits_base(td_base, &module.key.name, registry);
                    let prim = if is_enum {
                        atom("string")
                    } else if is_bits {
                        atom(bits_typedef_primitive(td_base, &module.key.name, registry))
                    } else {
                        // Resolve IETF/ConfD override from the base type chain (e.g. dscp→unsignedByte)
                        resolve_typedef_primitive(td_base, &module.key.name, registry, 16)
                    };
                    return TypeInfo {
                        exs_type: ns_pair(&module.namespace, type_arg),
                        primitive_type: prim,
                        extra_exs_flags: if is_enum { F_EXS_IS_ENUMERATION } else { 0 },
                        is_typedef: true, typedef_defining_module: Some(module.key.name.clone()),
                        is_enum_base: is_enum,
                    };
                }
            }
        }
        return builtin;
    };

    // If it's a built-in name (shouldn't have a prefix, but guard just in case)
    let builtin = resolve_builtin_type(local);
    // Only fall back to builtin if the result is NOT the default fallback (string)
    // We check by seeing if prefix actually matches a module import.
    
    // Resolve the prefix to the importing module to get its prefix_map
    let node_module = registry.resolve_import(node_module_name, None);
    let module_name = node_module
        .as_ref()
        .and_then(|m| m.prefix_map.get(prefix).cloned());

    let module_name = match module_name {
        Some(n) => n,
        None => {
            // Prefix not found — maybe it's the module's own prefix
            if let Some(m) = &node_module {
                if m.prefix == prefix {
                    // Self-reference: type defined in this module
                    let (is_enum, is_bits) = if let Some(td) = m.typedefs.get(local) {
                        let td_base = td.type_stmt.arg.as_deref().unwrap_or("string");
                        let e = is_enumeration_base(td_base, &m.key.name, registry);
                        let b = !e && is_bits_base(td_base, &m.key.name, registry);
                        (e, b)
                    } else { (false, false) };
                    let prim = if is_enum {
                        atom("string")
                    } else if is_bits {
                        let td_base = m.typedefs.get(local)
                            .and_then(|td| td.type_stmt.arg.as_deref().map(|s| s.to_owned()))
                            .unwrap_or_default();
                        atom(bits_typedef_primitive(&td_base, &m.key.name, registry))
                    } else {
                        // Resolve IETF/ConfD override from the base type chain
                        if let Some(td) = m.typedefs.get(local) {
                            let td_base = td.type_stmt.arg.as_deref().unwrap_or("string");
                            resolve_typedef_primitive(td_base, &m.key.name, registry, 16)
                        } else {
                            undefined()
                        }
                    };
                    return TypeInfo {
                        exs_type: ns_pair(&m.namespace, local),
                        primitive_type: prim,
                        extra_exs_flags: if is_enum { F_EXS_IS_ENUMERATION } else { builtin.extra_exs_flags },
                        is_typedef: true, typedef_defining_module: Some(m.key.name.clone()),
                        is_enum_base: is_enum,
                    };
                }
            }
            return builtin;
        }
    };

    // Check if this is actually a YANG built-in type name from a known module
    let typedef_module = registry.resolve_import(&module_name, None);
    let ns = match &typedef_module {
        Some(m) => m.namespace.clone(),
        None => return builtin,
    };

    // Check for special IETF/confd primitive_type overrides
    let prim = ietf_primitive_type(&module_name, local);
    // If no IETF primitive override, check if this typedef resolves to bits
    let prim = if matches!(prim, eetf::Term::Atom(ref a) if a.name == "undefined") {
        if let Some(td_mod) = &typedef_module {
            if let Some(td) = td_mod.typedefs.get(local) {
                let td_base = td.type_stmt.arg.as_deref().unwrap_or("string");
                if is_bits_base(td_base, &module_name, registry) {
                    atom(bits_typedef_primitive(td_base, &module_name, registry))
                } else {
                    prim
                }
            } else { prim }
        } else { prim }
    } else { prim };

    // Check if external module typedef resolves to enum (for PARSE_DEFAULT detection only;
    // extra_exs_flags stays as builtin's since the EXS record does NOT use F_EXS_IS_ENUMERATION
    // for cross-module enum typedefs, matching yanger's behavior).
    let is_enum = typedef_module.as_ref()
        .and_then(|m| m.typedefs.get(local))
        .map(|td| {
            let base = td.type_stmt.arg.as_deref().unwrap_or("string");
            is_enumeration_base(base, &module_name, registry)
        })
        .unwrap_or(false);

    TypeInfo {
        exs_type: ns_pair(&ns, local),
        primitive_type: prim,
        extra_exs_flags: builtin.extra_exs_flags,
        is_typedef: true, typedef_defining_module: Some(module_name),
        is_enum_base: is_enum,
    }
}

/// Return a special `primitive_type` atom for well-known IETF/ConfD typedef names,
/// or `undefined()` for all others.
fn ietf_primitive_type(module_name: &str, local: &str) -> Term {
    match (module_name, local) {
        // ietf-inet-types primitives
        ("ietf-inet-types", "ip-address") => atom("inetAddressIP"),
        ("ietf-inet-types", "ipv4-address") => atom("inetAddressIPv4"),
        ("ietf-inet-types", "ipv6-address") => atom("inetAddressIPv6"),
        ("ietf-inet-types", "ip-prefix") => atom("ipPrefix"),
        ("ietf-inet-types", "ipv4-prefix") => atom("ipv4Prefix"),
        ("ietf-inet-types", "ipv6-prefix") => atom("ipv6Prefix"),
        ("ietf-inet-types", "domain-name") => atom("inetAddressDNS"),
        ("ietf-inet-types", "host") => atom("inetAddressDNS"),
        ("ietf-inet-types", "port-number") => atom("unsignedShort"),
        ("ietf-inet-types", "as-number") => atom("unsignedInt"),
        ("ietf-inet-types", "ip-address-no-zone") => atom("inetAddressIP"),
        ("ietf-inet-types", "ipv4-address-no-zone") => atom("inetAddressIPv4"),
        ("ietf-inet-types", "ipv6-address-no-zone") => atom("inetAddressIPv6"),
        ("ietf-inet-types", "dscp") => atom("unsignedByte"),
        ("ietf-inet-types", "ipv6-flow-label") => atom("unsignedInt"),
        ("ietf-inet-types", "zone-index") => atom("unsignedInt"),
        // ietf-yang-types primitives
        ("ietf-yang-types", "counter32") => atom("Counter32"),
        ("ietf-yang-types", "counter64") => atom("Counter64"),
        ("ietf-yang-types", "gauge32") => atom("Gauge32"),
        ("ietf-yang-types", "gauge64") => atom("Counter64"),
        // timeticks has no special CONFD primitive — it falls back to its base uint32 type
        ("ietf-yang-types", "timeticks") => atom("unsignedInt"),
        ("ietf-yang-types", "timestamp") => atom("unsignedInt"),
        ("ietf-yang-types", "object-identifier") => atom("OBJECT IDENTIFIER"),
        ("ietf-yang-types", "phys-address") => atom("hexList"),
        ("ietf-yang-types", "mac-address") => atom("hexList"),
        ("ietf-yang-types", "hex-string") => atom("hexString"),
        ("ietf-yang-types", "dotted-quad") => atom("dottedQuad"),
        ("ietf-yang-types", "xpath1.0") => atom("string"),
        ("ietf-yang-types", "date-and-time") => atom("dateTime"),
        ("ietf-yang-types", "uuid") => atom("string"),
        ("ietf-yang-types", "yang-identifier") => atom("string"),
        // tailf-common primitives
        ("tailf-common", "md5-digest-string") => atom("string"),
        ("tailf-common", "sha-256-digest-string") => atom("string"),
        ("tailf-common", "aes-cfb-128-encrypted-string") => atom("string"),
        ("tailf-common", "aes-256-cfb-128-encrypted-string") => atom("string"),
        ("tailf-common", "des3-cbc-encrypted-string") => atom("string"),
        ("tailf-common", "hex-list") => atom("string"),
        ("tailf-common", "octet-list") => atom("string"),
        ("tailf-common", "ip-address-and-prefix-length") => atom("inetAddressIPv4"),
        ("tailf-common", "ipv4-address-and-prefix-length") => atom("inetAddressIPv4"),
        ("tailf-common", "ipv6-address-and-prefix-length") => atom("inetAddressIPv6"),
        ("tailf-common", "size") => atom("unsignedInt"),
        ("tailf-common", "stat") => atom("unsignedInt"),
        _ => undefined(),
    }
}

/// Resolve the primitive_type for a typedef's base type by following the typedef chain.
/// ONLY returns a non-undefined primitive when the chain hits an IETF/ConfD known override
/// (via `ietf_primitive_type`). Generic non-IETF typedefs (including numeric builtins) return `undefined()`.
fn resolve_typedef_primitive(
    type_arg: &str,
    module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
    depth: usize,
) -> Term {
    if depth == 0 { return undefined(); }

    // Strip to prefix/local
    let (prefix, local) = if let Some(pos) = type_arg.find(':') {
        (&type_arg[..pos], &type_arg[pos + 1..])
    } else {
        ("", type_arg)
    };

    let td_module = if prefix.is_empty() {
        registry.resolve_import(module_name, None)
    } else {
        registry.resolve_import(module_name, None)
            .and_then(|m| m.prefix_map.get(prefix).cloned())
            .and_then(|mn| registry.resolve_import(&mn, None))
    };

    if let Some(m) = td_module {
        // Check IETF/ConfD primitive override for this type name in its defining module
        let ietf_prim = ietf_primitive_type(&m.key.name, local);
        if !matches!(&ietf_prim, eetf::Term::Atom(a) if a.name == "undefined") {
            return ietf_prim;
        }
        // If it's a YANG built-in, stop: no IETF override means undefined for typedef chains
        if is_yang_builtin_type(local) {
            return undefined();
        }
        // Follow the typedef chain
        if let Some(td) = m.typedefs.get(local) {
            let base = td.type_stmt.arg.as_deref().unwrap_or("string");
            return resolve_typedef_primitive(base, &m.key.name, registry, depth - 1);
        }
    }
    undefined()
}

fn is_yang_builtin_type(name: &str) -> bool {
    matches!(
        name,
        "string" | "boolean" | "int8" | "int16" | "int32" | "int64"
            | "uint8" | "uint16" | "uint32" | "uint64" | "decimal64"
            | "binary" | "empty" | "instance-identifier" | "identityref"
            | "leafref" | "enumeration" | "bits" | "union"
    )
}

/// Returns true if the given `type_arg` ultimately resolves to `enumeration`.
/// Follows typedef chains up to a depth limit.
fn is_enumeration_base(
    type_arg: &str,
    module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
) -> bool {
    is_enumeration_base_depth(type_arg, module_name, registry, 16)
}

fn is_enumeration_base_depth(
    type_arg: &str,
    module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
    depth: usize,
) -> bool {
    if depth == 0 { return false; }
    if type_arg == "enumeration" { return true; }
    if type_arg == "bits" || type_arg == "union" || type_arg == "empty" { return false; }

    // If it contains a colon, it's a prefixed reference
    let (prefix, local) = if let Some(pos) = type_arg.find(':') {
        (&type_arg[..pos], &type_arg[pos + 1..])
    } else {
        ("", type_arg)
    };

    // Get the module where the typedef lives
    let td_module = if prefix.is_empty() {
        registry.resolve_import(module_name, None)
    } else {
        // Resolve prefix via the current module's prefix_map
        registry.resolve_import(module_name, None)
            .and_then(|m| m.prefix_map.get(prefix).cloned())
            .and_then(|mn| registry.resolve_import(&mn, None))
    };

    if let Some(m) = td_module {
        if let Some(td) = m.typedefs.get(local) {
            let base = td.type_stmt.arg.as_deref().unwrap_or("string");
            return is_enumeration_base_depth(base, &m.key.name, registry, depth - 1);
        }
    }
    false
}

/// Returns true if the given `type_arg` ultimately resolves to `bits`.
/// Follows typedef chains up to a depth limit.
pub fn is_bits_base(
    type_arg: &str,
    module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
) -> bool {
    is_bits_base_depth(type_arg, module_name, registry, 16)
}

fn is_bits_base_depth(
    type_arg: &str,
    module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
    depth: usize,
) -> bool {
    if depth == 0 { return false; }
    if type_arg == "bits" { return true; }
    if type_arg == "enumeration" || type_arg == "union" || type_arg == "empty" { return false; }

    let (prefix, local) = if let Some(pos) = type_arg.find(':') {
        (&type_arg[..pos], &type_arg[pos + 1..])
    } else {
        ("", type_arg)
    };

    let td_module = if prefix.is_empty() {
        registry.resolve_import(module_name, None)
    } else {
        registry.resolve_import(module_name, None)
            .and_then(|m| m.prefix_map.get(prefix).cloned())
            .and_then(|mn| registry.resolve_import(&mn, None))
    };

    if let Some(m) = td_module {
        if let Some(td) = m.typedefs.get(local) {
            let base = td.type_stmt.arg.as_deref().unwrap_or("string");
            return is_bits_base_depth(base, &m.key.name, registry, depth - 1);
        }
    }
    false
}

/// Compute the bits primitive atom for a typedef that resolves to `bits`.
/// Walks the typedef chain to find the innermost `bits` type and computes max_pos.
pub fn bits_typedef_primitive(
    type_arg: &str,
    module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
) -> &'static str {
    let max_pos = bits_typedef_max_pos(type_arg, module_name, registry, 16);
    crate::thash::bits_primitive_atom(max_pos)
}

fn bits_typedef_max_pos(
    type_arg: &str,
    module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
    depth: usize,
) -> u32 {
    if depth == 0 || type_arg == "bits" {
        return 0;
    }

    let (prefix, local) = if let Some(pos) = type_arg.find(':') {
        (&type_arg[..pos], &type_arg[pos + 1..])
    } else {
        ("", type_arg)
    };

    let td_module = if prefix.is_empty() {
        registry.resolve_import(module_name, None)
    } else {
        registry.resolve_import(module_name, None)
            .and_then(|m| m.prefix_map.get(prefix).cloned())
            .and_then(|mn| registry.resolve_import(&mn, None))
    };

    if let Some(m) = td_module {
        if let Some(td) = m.typedefs.get(local) {
            let base = td.type_stmt.arg.as_deref().unwrap_or("string");
            if base == "bits" {
                use yangest_core::ast::{BuiltInKeyword, Keyword};
                let mut max_pos: u32 = 0;
                let mut next_pos: u32 = 0;
                for sub in &td.type_stmt.substmts {
                    if matches!(&sub.keyword, Keyword::BuiltIn(BuiltInKeyword::Bit)) {
                        let pos = sub.substmts.iter()
                            .find(|s| matches!(&s.keyword, Keyword::BuiltIn(BuiltInKeyword::Position)))
                            .and_then(|s| s.arg.as_deref())
                            .and_then(|s| s.trim().parse::<u32>().ok())
                            .unwrap_or(next_pos);
                        next_pos = pos + 1;
                        if pos > max_pos { max_pos = pos; }
                    }
                }
                return max_pos;
            } else {
                return bits_typedef_max_pos(base, &m.key.name, registry, depth - 1);
            }
        }
    }
    0
}

/// Returns true if the given `type_arg` ultimately resolves to `union`.
/// For builtin modules (ietf-inet-types etc.), checks a hardcoded list of known union types.
/// For registry modules, follows typedef chains.
pub fn is_union_base(
    type_arg: &str,
    module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
) -> bool {
    is_union_base_depth(type_arg, module_name, registry, 16)
}

fn is_union_base_depth(
    type_arg: &str,
    module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
    depth: usize,
) -> bool {
    if depth == 0 {
        return false;
    }
    if type_arg == "union" {
        return true;
    }
    if matches!(
        type_arg,
        "string"
            | "boolean"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "decimal64"
            | "binary"
            | "leafref"
            | "identityref"
            | "instance-identifier"
            | "empty"
            | "bits"
            | "enumeration"
    ) {
        return false;
    }

    let (prefix, local) = if let Some(pos) = type_arg.find(':') {
        (&type_arg[..pos], &type_arg[pos + 1..])
    } else {
        ("", type_arg)
    };

    let td_module_name = if prefix.is_empty() {
        Some(module_name.to_string())
    } else {
        registry
            .resolve_import(module_name, None)
            .and_then(|m| m.prefix_map.get(prefix).cloned())
    };

    if let Some(ref mn) = td_module_name {
        // Hardcoded union types for builtin modules not present in registry.
        match (mn.as_str(), local) {
            (
                "ietf-inet-types",
                "ip-address" | "ip-prefix" | "ip-address-no-zone" | "host",
            ) => return true,
            _ => {}
        }
        if let Some(m) = registry.resolve_import(mn, None) {
            if let Some(td) = m.typedefs.get(local) {
                let base = td.type_stmt.arg.as_deref().unwrap_or("string");
                return is_union_base_depth(base, &m.key.name, registry, depth - 1);
            }
        }
    }
    false
}

/// Returns true if the given `type_arg` ultimately resolves to `empty`.
pub fn is_empty_base(
    type_arg: &str,
    module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
) -> bool {
    is_empty_base_depth(type_arg, module_name, registry, 16)
}

fn is_empty_base_depth(
    type_arg: &str,
    module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
    depth: usize,
) -> bool {
    if depth == 0 {
        return false;
    }
    if type_arg == "empty" {
        return true;
    }
    if matches!(
        type_arg,
        "union"
            | "enumeration"
            | "bits"
            | "string"
            | "boolean"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "decimal64"
            | "binary"
            | "leafref"
            | "identityref"
            | "instance-identifier"
    ) {
        return false;
    }

    let (prefix, local) = if let Some(pos) = type_arg.find(':') {
        (&type_arg[..pos], &type_arg[pos + 1..])
    } else {
        ("", type_arg)
    };

    let td_module = if prefix.is_empty() {
        registry.resolve_import(module_name, None)
    } else {
        registry
            .resolve_import(module_name, None)
            .and_then(|m| m.prefix_map.get(prefix).cloned())
            .and_then(|mn| registry.resolve_import(&mn, None))
    };

    if let Some(m) = td_module {
        if let Some(td) = m.typedefs.get(local) {
            let base = td.type_stmt.arg.as_deref().unwrap_or("string");
            return is_empty_base_depth(base, &m.key.name, registry, depth - 1);
        }
    }
    false
}
