//! eetf `Term` construction helpers for FXS records.
//!
//! Erlang records are encoded as tuples where the first element is the record
//! name atom.  Field ordering must exactly match the Erlang `.hrl` definitions.

use eetf::{Atom, BigInteger, Binary, FixInteger, ImproperList, List, Term, Tuple};
use num_bigint::BigInt;

// ---------------------------------------------------------------------------
// Primitive term builders
// ---------------------------------------------------------------------------

pub fn atom(s: &str) -> Term {
    Term::from(Atom { name: s.to_string() })
}

/// Encode a Rust `&str` as an Erlang charlist (list of char code integers).
///
/// In Erlang, double-quoted strings like `"abc"` are charlists — lists of
/// integers — not atoms or binaries.  Uses ETF `STRING_EXT` (0x6B) encoding
/// via a Vec of FixInteger elements, which eetf will encode as a standard list.
pub fn charlist(s: &str) -> Term {
    let elems: Vec<Term> = s
        .chars()
        .map(|c| Term::from(FixInteger { value: c as i32 }))
        .collect();
    if elems.is_empty() {
        nil()
    } else {
        Term::from(List { elements: elems })
    }
}

pub fn nil() -> Term {
    Term::from(List::nil())
}

pub fn int(n: i32) -> Term {
    Term::from(FixInteger { value: n })
}

pub fn int64(n: i64) -> Term {
    if n >= i32::MIN as i64 && n <= i32::MAX as i64 {
        Term::from(FixInteger { value: n as i32 })
    } else {
        Term::from(BigInteger { value: BigInt::from(n) })
    }
}

pub fn uint(n: u32) -> Term {
    if n <= i32::MAX as u32 {
        Term::from(FixInteger { value: n as i32 })
    } else {
        Term::from(BigInteger { value: BigInt::from(n) })
    }
}

/// Encode an arbitrary-precision non-negative integer (for large flag bitmasks).
pub fn bigint(n: u128) -> Term {
    if n <= i32::MAX as u128 {
        Term::from(FixInteger { value: n as i32 })
    } else {
        Term::from(BigInteger { value: BigInt::from(n) })
    }
}

pub fn binary(bytes: Vec<u8>) -> Term {
    Term::from(Binary { bytes })
}

pub fn binary_str(s: &str) -> Term {
    binary(s.as_bytes().to_vec())
}

pub fn tuple(elements: Vec<Term>) -> Term {
    Term::from(Tuple { elements })
}

pub fn list(items: Vec<Term>) -> Term {
    if items.is_empty() {
        nil()
    } else {
        Term::from(List { elements: items })
    }
}

pub fn undefined() -> Term {
    atom("undefined")
}

pub fn boolean(b: bool) -> Term {
    atom(if b { "true" } else { "false" })
}

// ---------------------------------------------------------------------------
// FXS record constructors
// ---------------------------------------------------------------------------

/// Build the `{fxs_header, key, id, uri, type, exported_agents, prefix,
///             id_hash_value, ns_dependencies, checksum, flags, mountpoint, tid,
///             snmp_info, yang_header, merged_revisions, model_sizes,
///             cdb_checksum, augments, sections, mount_id, xmlns,
///             ns_to_prefix_maps}` tuple (22 fields + record name = 23 elements).
#[allow(clippy::too_many_arguments)]
pub fn make_fxs_header(
    id: Term,
    uri: Term,
    xmlns: Term,
    typ: Term,
    exported_agents: Term,
    prefix: Term,
    id_hash_value: Term,
    ns_dependencies: Term,
    checksum: Term,
    flags: Term,
    yang_header: Term,
    model_sizes: Term,
    cdb_checksum: Term,
    augments: Term,
    sections: Term,
    mount_id: Term,
    ns_to_prefix_maps: Term,
) -> Term {
    tuple(vec![
        atom("fxs_header"),
        atom("fxs_header"), // key field = fxs_header
        id,
        uri,
        typ,
        exported_agents,
        prefix,
        id_hash_value,
        ns_dependencies,
        checksum,
        flags,
        undefined(), // mountpoint (not used, kept for upgrade compat)
        undefined(), // tid (ets table name during upgrade)
        nil(),       // snmp_info
        yang_header,
        nil(),       // merged_revisions
        model_sizes,
        cdb_checksum,
        augments,
        sections,
        mount_id,
        xmlns,
        ns_to_prefix_maps,
    ])
}

/// Build the `{yang_header, yang_version, revision, features, deviations,
///             module_name, flags, submodules, imports}` tuple
/// (8 fields + record name = 9 elements).
pub fn make_yang_header(
    yang_version: Term,
    revision: Term,
    features: Term,
    deviations: Term,
    module_name: Term,
    flags: Term,
    submodules: Term,
    imports: Term,
) -> Term {
    tuple(vec![
        atom("yang_header"),
        yang_version,
        revision,
        features,
        deviations,
        module_name,
        flags,
        submodules,
        imports,
    ])
}

/// Build a `{cs, tagpath, htag, namespace, hnamespace, exs, keys, flags,
///           dbm, dba, validatemfas, actions, cmp, hooks, hidden, notifs,
///           symlink, extra, default_ref, secondary_indices, cli_flags,
///           oper_dbm, oper_dba, structures}` tuple
/// (23 fields + record name = 24 elements).
#[allow(clippy::too_many_arguments)]
pub fn make_cs(
    tagpath: Term,
    htag: Term,
    namespace: Term,
    hnamespace: Term,
    exs: Term,
    keys: Term,
    flags: Term,
    dbm: Term,
    dba: Term,
    validatemfas: Term,
    actions: Term,
    cmp: Term,
    hooks: Term,
    hidden: Term,
    notifs: Term,
    symlink: Term,
    extra: Term,
    default_ref: Term,
    secondary_indices: Term,
    cli_flags: Term,
    oper_dbm: Term,
    oper_dba: Term,
    structures: Term,
) -> Term {
    tuple(vec![
        atom("cs"),
        tagpath,
        htag,
        namespace,
        hnamespace,
        exs,
        keys,
        flags,
        dbm,
        dba,
        validatemfas,
        actions,
        cmp,
        hooks,
        hidden,
        notifs,
        symlink,
        extra,
        default_ref,
        secondary_indices,
        cli_flags,
        oper_dbm,
        oper_dba,
        structures,
    ])
}

/// Build an `{exs, tagpath, type, primitive_type, default, attrs,
///            min_occurs, max_occurs, children, flags, extra}` tuple
/// (10 fields + record name = 11 elements).
#[allow(clippy::too_many_arguments)]
pub fn make_exs(
    tagpath: Term,
    typ: Term,
    primitive_type: Term,
    default: Term,
    attrs: Term,
    min_occurs: Term,
    max_occurs: Term,
    children: Term,
    flags: Term,
    extra: Term,
) -> Term {
    tuple(vec![
        atom("exs"),
        tagpath,
        typ,
        primitive_type,
        default,
        attrs,
        min_occurs,
        max_occurs,
        children,
        flags,
        extra,
    ])
}

/// Build a `{hash, name, type, hash_value, flags, code_name}` tuple
/// (5 fields + record name = 6 elements).
pub fn make_hash_record(
    name: Term,
    typ: Term,
    hash_value: Term,
    flags: Term,
    code_name: Term,
) -> Term {
    tuple(vec![atom("hash"), name, typ, hash_value, flags, code_name])
}

/// Build the `{callpoint_info, key, info}` tuple (2 fields + record name = 3 elements).
///
/// The `key` field is always the atom `callpoint_info` (same as record name).
pub fn make_callpoint_info(info: Term) -> Term {
    tuple(vec![atom("callpoint_info"), atom("callpoint_info"), info])
}

/// Build an Erlang tagpath list from a bottom-up path (innermost first).
///
/// In FXS, `exs.tagpath = [node_name | parent_names...]` where names are atoms.
/// For the root cs, the tagpath is `[]`.
pub fn make_tagpath(path: &[&str]) -> Term {
    list(path.iter().map(|s| atom(s)).collect())
}

/// Build an Erlang improper list cell `[head | tail]` where `tail` is a non-nil term.
///
/// Used for augmented node tagpath elements where `[AugmentingNS | name]` encodes
/// a cross-namespace reference (mirroring yanger's `qtag(Ns, Name)` when Ns != def_ns).
pub fn improper_list_pair(head: Term, tail: Term) -> Term {
    Term::from(ImproperList {
        elements: vec![head],
        last: Box::new(tail),
    })
}

/// Build a BigInteger term from a `num_bigint::BigInt` value.
///
/// Used for large CLI flag bitmasks that exceed u128 range.
pub fn bigint_bigint(n: num_bigint::BigInt) -> Term {
    use std::convert::TryFrom;
    if let Ok(v) = i32::try_from(&n) {
        return Term::from(FixInteger { value: v });
    }
    Term::from(BigInteger { value: n })
}

/// Build a `{load_augment, target_tagpath, target_htag, target_namespace,
///   target_hnamespace, target_type, children, actions, notifs, keypath_length,
///   required_set_flags, required_clr_flags, propagate_flags_up, propagate_extra_up,
///   flags}` tuple (14 fields + record name = 15 elements).
#[allow(clippy::too_many_arguments)]
pub fn make_load_augment(
    target_tagpath: Term,
    target_htag: Term,
    target_namespace: Term,
    target_hnamespace: Term,
    target_type: Term,
    children: Term,
    actions: Term,
    notifs: Term,
    keypath_length: Term,
    required_set_flags: Term,
    required_clr_flags: Term,
    propagate_flags_up: Term,
    propagate_extra_up: Term,
    flags: Term,
) -> Term {
    tuple(vec![
        atom("load_augment"),
        target_tagpath,
        target_htag,
        target_namespace,
        target_hnamespace,
        target_type,
        children,
        actions,
        notifs,
        keypath_length,
        required_set_flags,
        required_clr_flags,
        propagate_flags_up,
        propagate_extra_up,
        flags,
    ])
}
