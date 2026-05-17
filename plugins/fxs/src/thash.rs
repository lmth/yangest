//! t<hash> synthetic type generation.
//!
//! yanger_fxs generates anonymous types for inline type constraints (length, pattern, range)
//! and for leaf-list nodes.  Each anonymous type gets a name `t<N>` where N is computed by
//! `erlang:phash2(term_to_binary({ExsType0, LoadType, Misc}, [{minor_version, 1}]), 0xffffffff)`.
//!
//! The LoadType is non-undefined when `AllLoadTypeFlags != 0`:
//! - Inline enum types: `AllLoadTypeFlags = F_LOAD_FXS_IS_ENUMERATION = 2`, so LoadType is
//!   `#load_type{base={xsd_ns,string}, flags=2, data=Facets}`.
//! - Inline bits types: `AllLoadTypeFlags = F_LOAD_FXS_IS_BITS = 64`, so LoadType is
//!   `#load_type{base=undefined, flags=64, primitive=PrimAtom, data=BitsRec}`.
//! - Restriction types (string/int with pattern/length/range): `AllLoadTypeFlags = 0`,
//!   so `LoadType = undefined`.
//!
//! **Critical**: Erlang's `term_to_binary/2` in OTP 29 uses ATOM_EXT (tag 100) for atoms,
//! even with `minor_version=1` (which only affects float encoding).  The `eetf` crate uses
//! ATOM_UTF8_EXT (tag 119), so we cannot use eetf for hash computation.  We use a direct
//! byte-encoding approach here.

use std::collections::HashMap;

use eetf::Term;

use crate::hash::phash2_bytes;
use crate::terms::{atom, binary, charlist, int, int64, list, nil, tuple, undefined};

// ---------------------------------------------------------------------------
// Direct old-style ETF byte encoder
// ---------------------------------------------------------------------------

/// Encode an Erlang atom as ATOM_EXT (tag 100) with 2-byte length.
fn push_atom(buf: &mut Vec<u8>, name: &str) {
    let bytes = name.as_bytes();
    debug_assert!(bytes.len() <= 255, "atom too long: {name}");
    buf.push(100); // ATOM_EXT
    buf.push(0);
    buf.push(bytes.len() as u8);
    buf.extend_from_slice(bytes);
}

/// Encode an Erlang string (list of bytes) as STRING_EXT (tag 107) with 2-byte length.
/// This is how `term_to_binary("bp-su", [{minor_version,1}])` encodes an Erlang string.
fn push_string(buf: &mut Vec<u8>, s: &[u8]) {
    buf.push(107); // STRING_EXT
    let len = s.len() as u16;
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(s);
}

/// Encode an integer in the most compact old-style ETF form.
fn push_int(buf: &mut Vec<u8>, n: i64) {
    if n >= 0 && n <= 255 {
        buf.push(97); // SMALL_INTEGER_EXT
        buf.push(n as u8);
    } else if n >= i32::MIN as i64 && n <= i32::MAX as i64 {
        buf.push(98); // INTEGER_EXT
        buf.extend_from_slice(&(n as i32).to_be_bytes());
    } else {
        let (sign, val) = if n < 0 {
            (1u8, (-(n)) as u64)
        } else {
            (0u8, n as u64)
        };
        let mut nbytes = 0u8;
        let mut v = val;
        while v > 0 {
            v >>= 8;
            nbytes += 1;
        }
        if nbytes == 0 {
            nbytes = 1;
        }
        buf.push(110); // SMALL_BIG_EXT
        buf.push(nbytes);
        buf.push(sign);
        let mut remaining = val;
        for _ in 0..nbytes {
            buf.push((remaining & 0xff) as u8);
            remaining >>= 8;
        }
    }
}

/// Encode nil (empty list) as NIL_EXT (tag 106).
fn push_nil(buf: &mut Vec<u8>) {
    buf.push(106);
}

/// Start a tuple of arity N.
fn push_tuple(buf: &mut Vec<u8>, arity: usize) {
    if arity <= 255 {
        buf.push(104); // SMALL_TUPLE_EXT
        buf.push(arity as u8);
    } else {
        buf.push(105); // LARGE_TUPLE_EXT
        buf.extend_from_slice(&(arity as u32).to_be_bytes());
    }
}

/// Start a list of length N (followed by elements, then NIL_EXT tail).
fn push_list_header(buf: &mut Vec<u8>, len: usize) {
    if len == 0 {
        buf.push(106); // NIL_EXT
    } else {
        buf.push(108); // LIST_EXT
        buf.extend_from_slice(&(len as u32).to_be_bytes());
    }
}

fn push_binary(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.push(109); // BINARY_EXT
    buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(bytes);
}

// ---------------------------------------------------------------------------
// exs_type record encoding for hash computation
// ---------------------------------------------------------------------------
//
// The exs_type record is a 13-element tuple:
//   {exs_type, name, type, lex_fn, val_fn, v2v_fn, str_fn, strcli_fn, derivation, desc, check_val, extra, flags}
// At hash time: name=undefined, all fns=undefined, desc=[], check_val=undefined, extra=[]

fn push_exs_type_header(buf: &mut Vec<u8>, flags: u32) {
    // This function should NOT be called standalone; instead use the per-type functions below.
    // It provides the common boilerplate: record tag, undefined name, 6 undefined fns.
    push_tuple(buf, 13);
    push_atom(buf, "exs_type");
    push_atom(buf, "undefined"); // name = undefined at hash time
    push_atom(buf, "derived"); // type
    push_atom(buf, "undefined"); // lexical_value_fun
    push_atom(buf, "undefined"); // value_fun
    push_atom(buf, "undefined"); // value2value_fun
    push_atom(buf, "undefined"); // string_fun
    push_atom(buf, "undefined"); // string_cli_fun
    // derivation is pushed next by the caller
    let _ = flags; // reminder: caller must push desc, check_val, extra, flags after derivation
}

fn push_exs_type_tail(buf: &mut Vec<u8>, flags: u32) {
    push_nil(buf); // desc = []
    push_atom(buf, "undefined"); // check_value
    push_nil(buf); // extra = []
    push_int(buf, flags as i64); // flags
}

/// Encode the complete exs_type for a list derivation:
/// `{exs_type, undefined, derived, ..., {list, {base_ns, base_name}}, [], undefined, [], 19}`
fn push_list_exs_type(buf: &mut Vec<u8>, base_ns: &str, base_name: &str) {
    push_exs_type_header(buf, 19);
    // derivation = {list, {base_ns, base_name}}
    push_tuple(buf, 2);
    push_atom(buf, "list");
    push_tuple(buf, 2);
    push_atom(buf, base_ns);
    push_atom(buf, base_name);
    push_exs_type_tail(buf, 19);
}

/// Encode the complete exs_type for a unique_list restriction:
/// `{exs_type, undefined, ..., {restriction, {list_ns, list_name}, [{unique_list,...}]}, [], undefined, [], 2}`
fn push_unique_list_exs_type(
    buf: &mut Vec<u8>,
    list_ns: &str,
    list_name: &str,
    min: u64,
    max: Option<u64>,
) {
    push_exs_type_header(buf, 2);
    // derivation = {restriction, {list_ns, list_name}, [{unique_list, undefined, false, [], [], min, max}]}
    push_tuple(buf, 3);
    push_atom(buf, "restriction");
    push_tuple(buf, 2);
    push_atom(buf, list_ns);
    push_atom(buf, list_name);
    // facets list with ONE unique_list
    push_list_header(buf, 1);
    push_tuple(buf, 7);
    push_atom(buf, "unique_list");
    push_atom(buf, "undefined"); // value (unused)
    push_atom(buf, "false"); // fixed
    push_nil(buf); // error_message
    push_nil(buf); // error_app_tag
    push_int(buf, min as i64); // min_occurs
    match max {
        Some(n) => push_int(buf, n as i64),
        None => push_atom(buf, "unbounded"),
    }
    push_nil(buf); // list tail
    push_exs_type_tail(buf, 2);
}

/// Encode a single `#enumeration{value, code_name, hash_value}` facet to ETF bytes.
///
/// `code_name_bytes`: `None` → `false` atom; `Some(b)` → binary.
fn push_enum_facet(buf: &mut Vec<u8>, name: &[u8], value: i64, code_name_bytes: Option<&[u8]>) {
    push_tuple(buf, 4);
    push_atom(buf, "enumeration");
    push_binary(buf, name); // value field = binary name
    match code_name_bytes {
        None => push_atom(buf, "false"),
        Some(b) => push_binary(buf, b),
    }
    push_int(buf, value);
}

/// Encode the complete exs_type for an inline enumeration restriction:
/// `{exs_type, undefined, derived, ..., {restriction, {xsd_ns,string}, [enum_facets]}, [], undefined, [], 2}`
///
/// `facets`: (name_bytes, code_name_bytes, value) in REVERSE YANG declaration order (foldl order).
fn push_enum_exs_type(buf: &mut Vec<u8>, facets: &[(Vec<u8>, Option<Vec<u8>>, i64)]) {
    let xsd_ns = "http://www.w3.org/2001/XMLSchema";
    push_exs_type_header(buf, 2);
    push_tuple(buf, 3);
    push_atom(buf, "restriction");
    push_tuple(buf, 2);
    push_atom(buf, xsd_ns);
    push_atom(buf, "string");
    push_list_header(buf, facets.len());
    for (name, code_name, value) in facets {
        push_enum_facet(buf, name, *value, code_name.as_deref());
    }
    if !facets.is_empty() {
        push_nil(buf);
    }
    push_exs_type_tail(buf, 2);
}

/// Build the eetf Term for a single enumeration facet (for writing to FXS).
pub fn enum_facet_eetf(name: &[u8], value: i64, code_name: Option<&[u8]>) -> Term {
    let cn = match code_name {
        None => atom("false"),
        Some(b) => Term::from(eetf::Binary { bytes: b.to_vec() }),
    };
    tuple(vec![
        atom("enumeration"),
        Term::from(eetf::Binary {
            bytes: name.to_vec(),
        }),
        cn,
        int(value as i32),
    ])
}

/// Build the eetf Term for the full enum exs_type record (for writing to FXS).
fn build_enum_exs_type_eetf(name: &str, facets: &[(Vec<u8>, Option<Vec<u8>>, i64)]) -> Term {
    let xsd_ns = "http://www.w3.org/2001/XMLSchema";
    let facet_terms: Vec<Term> = facets
        .iter()
        .map(|(n, cn, v)| enum_facet_eetf(n, *v, cn.as_deref()))
        .collect();
    let derivation = tuple(vec![
        atom("restriction"),
        tuple(vec![atom(xsd_ns), atom("string")]),
        list(facet_terms),
    ]);
    tuple(vec![
        atom("exs_type"),
        atom(name),
        atom("derived"),
        undefined(),
        undefined(),
        undefined(),
        undefined(),
        undefined(),
        derivation,
        nil(),
        undefined(),
        nil(),
        int(2), // F_EXS_TYPE_IS_GENERATED_BY_YANGER
    ])
}

/// Encode the complete exs_type for a restriction with pre-encoded facet bytes.
fn push_restriction_exs_type(
    buf: &mut Vec<u8>,
    base_ns: &str,
    base_name: &str,
    facet_bytes: &[Vec<u8>],
    flags: u32,
) {
    push_exs_type_header(buf, flags);
    push_tuple(buf, 3);
    push_atom(buf, "restriction");
    push_tuple(buf, 2);
    push_atom(buf, base_ns);
    push_atom(buf, base_name);
    push_list_header(buf, facet_bytes.len());
    for fb in facet_bytes {
        buf.extend_from_slice(fb);
    }
    if !facet_bytes.is_empty() {
        push_nil(buf);
    }
    push_exs_type_tail(buf, flags);
}

// ---------------------------------------------------------------------------
// Misc entries for type hash computation
// ---------------------------------------------------------------------------
//
// yanger_fxs's `mk_hash({ExsType, LoadType, Misc})` includes Misc in the hash.
// Misc contains doc entries for enum values / bit fields with tailf:info/description.
// For enum types (foldl semantics): Misc is in REVERSE YANG order.
// For bits types (foldr semantics): Misc is in YANG forward order.

/// A Misc entry included in the type hash {ExsType, LoadType, Misc}.
pub enum MiscEntry {
    /// #doc{name={enum, <<name>>}, desc=<<desc>>, flags, prompt=undefined}
    EnumDoc { name: Vec<u8>, desc: Vec<u8>, flags: u32 },
    /// #doc{name={bit, "name"}, desc=<<desc>>, flags, prompt=undefined}
    /// Note: bit names are encoded as STRING_EXT (char list), not BINARY_EXT.
    BitDoc { name: Vec<u8>, desc: Vec<u8>, flags: u32 },
    /// #code_name{name=<<name>>, code_name=<<code_name>>} — for bit fields with tailf:code-name
    BitCodeName { name: Vec<u8>, code_name: Vec<u8> },
}

fn push_misc_entry(buf: &mut Vec<u8>, entry: &MiscEntry) {
    match entry {
        MiscEntry::EnumDoc { name, desc, flags } => {
            push_tuple(buf, 5); // {doc, {enum, <<name>>}, <<desc>>, flags, undefined}
            push_atom(buf, "doc");
            push_tuple(buf, 2);
            push_atom(buf, "enum");
            push_binary(buf, name);
            push_binary(buf, desc);
            push_int(buf, *flags as i64);
            push_atom(buf, "undefined"); // prompt
        }
        MiscEntry::BitDoc { name, desc, flags } => {
            push_tuple(buf, 5); // {doc, {bit, "name"}, <<desc>>, flags, undefined}
            push_atom(buf, "doc");
            push_tuple(buf, 2);
            push_atom(buf, "bit");
            push_string(buf, name); // bit names are STRING_EXT (Erlang string/char-list)
            push_binary(buf, desc);
            push_int(buf, *flags as i64);
            push_atom(buf, "undefined"); // prompt
        }
        MiscEntry::BitCodeName { name, code_name } => {
            push_tuple(buf, 3); // {code_name, <<name>>, <<code_name>>}
            push_atom(buf, "code_name");
            push_binary(buf, name);
            push_binary(buf, code_name);
        }
    }
}

fn push_misc_list(buf: &mut Vec<u8>, misc: &[MiscEntry]) {
    if misc.is_empty() {
        push_nil(buf);
    } else {
        push_list_header(buf, misc.len());
        for entry in misc {
            push_misc_entry(buf, entry);
        }
        push_nil(buf);
    }
}

/// Compute phash2 of `{ExsType, undefined, Misc}`.
/// Used for restriction types (string, integer, binary) where LoadTypeFlags == 0,
/// and for pre-registered enum/bits types from unused groupings.
fn compute_thash_from_exs_bytes(exs_bytes: &[u8]) -> u32 {
    compute_thash_from_exs_bytes_misc(exs_bytes, &[])
}

fn compute_thash_from_exs_bytes_misc(exs_bytes: &[u8], misc: &[MiscEntry]) -> u32 {
    let mut buf = vec![131u8]; // ETF version tag
    push_tuple(&mut buf, 3);
    buf.extend_from_slice(exs_bytes);
    push_atom(&mut buf, "undefined"); // LoadType = undefined
    push_misc_list(&mut buf, misc);
    phash2_bytes(&buf)
}

/// Compute phash2 of `{ExsType, LoadType, []}` where LoadType is explicitly provided.
/// Used for union types (which have no Misc).
fn compute_thash_with_load_type(exs_bytes: &[u8], load_type_bytes: &[u8]) -> u32 {
    compute_thash_with_load_type_misc(exs_bytes, load_type_bytes, &[])
}

/// Compute phash2 of `{ExsType, LoadType, Misc}`.
/// Used for enum/bits where LoadTypeFlags != 0 (AllLoadTypeFlags = IS_ENUMERATION or IS_BITS).
fn compute_thash_with_load_type_misc(
    exs_bytes: &[u8],
    load_type_bytes: &[u8],
    misc: &[MiscEntry],
) -> u32 {
    let mut buf = vec![131u8]; // ETF version tag
    push_tuple(&mut buf, 3);
    buf.extend_from_slice(exs_bytes);
    buf.extend_from_slice(load_type_bytes);
    push_misc_list(&mut buf, misc);
    phash2_bytes(&buf)
}

/// Encode a simple load_type record with only base and flags set (all other fields undefined):
/// `{load_type, undefined, {base_ns, base_name}, flags, undefined, undefined, undefined, undefined}`
/// Used for leaf-list list/unique_list types when AllLoadTypeFlags != 0.
fn push_simple_load_type(buf: &mut Vec<u8>, base_ns: &str, base_name: &str, flags: u32) {
    push_tuple(buf, 8);
    push_atom(buf, "load_type");
    push_atom(buf, "undefined"); // name
    push_tuple(buf, 2);
    push_atom(buf, base_ns);
    push_atom(buf, base_name);
    push_int(buf, flags as i64);
    push_atom(buf, "undefined"); // default_str
    push_atom(buf, "undefined"); // default
    push_atom(buf, "undefined"); // primitive
    push_atom(buf, "undefined"); // data
}

fn build_simple_load_type_eetf(
    type_name: &str,
    base_ns: &str,
    base_name: &str,
    flags: u32,
) -> Term {
    tuple(vec![
        atom("load_type"),
        atom(type_name),
        tuple(vec![atom(base_ns), atom(base_name)]),
        int(flags as i32),
        undefined(),
        undefined(),
        undefined(),
        undefined(),
    ])
}

/// Encode a load_type record for an enum type.
/// `flags` is typically `F_LOAD_FXS_IS_ENUMERATION = 2` for direct leaves,
/// or `F_LOAD_FXS_IS_UNION = 4` when the enum is a member of a union type.
fn push_enum_load_type(
    buf: &mut Vec<u8>,
    facets: &[(Vec<u8>, Option<Vec<u8>>, i64)],
    flags: u32,
) {
    let xsd_ns = "http://www.w3.org/2001/XMLSchema";
    push_tuple(buf, 8); // load_type record = 8 elements (tag + 7 fields)
    push_atom(buf, "load_type");
    push_atom(buf, "undefined"); // name
    push_tuple(buf, 2); // base = {xsd_ns, string}
    push_atom(buf, xsd_ns);
    push_atom(buf, "string");
    push_int(buf, flags as i64);
    push_atom(buf, "undefined"); // default_str
    push_atom(buf, "undefined"); // default
    push_atom(buf, "undefined"); // primitive
    // data = facets list
    push_list_header(buf, facets.len());
    for (name, code_name, value) in facets {
        push_enum_facet(buf, name, *value, code_name.as_deref());
    }
    if !facets.is_empty() {
        push_nil(buf);
    }
}

/// Compute phash2 of `{ExsType, LoadType, Misc}` for an inline enumeration type.
/// `flags` = `F_LOAD_FXS_IS_ENUMERATION = 2` for direct leaves,
/// `F_LOAD_FXS_IS_UNION = 4` for enum members inside a union.
/// `misc` = doc entries for enum values with tailf:info/description (REVERSE YANG order).
fn compute_enum_thash(
    exs_bytes: &[u8],
    facets: &[(Vec<u8>, Option<Vec<u8>>, i64)],
    flags: u32,
    misc: &[MiscEntry],
) -> u32 {
    let mut lt_buf = Vec::new();
    push_enum_load_type(&mut lt_buf, facets, flags);
    compute_thash_with_load_type_misc(exs_bytes, &lt_buf, misc)
}

/// Build the eetf Term for the load_type record for an inline enum type.
fn build_enum_load_type_eetf(
    name: &str,
    facets: &[(Vec<u8>, Option<Vec<u8>>, i64)],
    flags: u32,
) -> Term {
    let xsd_ns = "http://www.w3.org/2001/XMLSchema";
    let facet_terms: Vec<Term> = facets
        .iter()
        .map(|(n, cn, v)| enum_facet_eetf(n, *v, cn.as_deref()))
        .collect();
    tuple(vec![
        atom("load_type"),
        atom(name),
        tuple(vec![atom(xsd_ns), atom("string")]),
        int(flags as i32),
        undefined(),
        undefined(),
        undefined(),
        list(facet_terms),
    ])
}

// ---------------------------------------------------------------------------
// Bits exs_type encoding
// ---------------------------------------------------------------------------

/// Compute the primitive type atom name for a bits type based on max bit position.
pub fn bits_primitive_atom(max_pos: u32) -> &'static str {
    if max_pos < 32 {
        "bits_type_32"
    } else if max_pos < 64 {
        "bits_type_64"
    } else {
        "bits_type_big"
    }
}

/// Encode the complete exs_type for a bits derivation (for hash computation).
/// Format: `{exs_type, undefined, derived, ..., {bits, [{Pos, "name"}...], Size}, ..., 2}`
fn push_bits_exs_type(buf: &mut Vec<u8>, fields: &[(u32, &str)], size: u32) {
    push_exs_type_header(buf, 2);
    push_bits_derivation(buf, fields, size);
    push_exs_type_tail(buf, 2);
}

fn push_bits_derivation(buf: &mut Vec<u8>, fields: &[(u32, &str)], size: u32) {
    push_tuple(buf, 3);
    push_atom(buf, "bits");
    push_list_header(buf, fields.len());
    for (pos, name) in fields {
        push_tuple(buf, 2);
        push_int(buf, *pos as i64);
        push_string(buf, name.as_bytes()); // Erlang string = STRING_EXT
    }
    if !fields.is_empty() {
        push_nil(buf);
    }
    push_int(buf, size as i64);
}

/// Encode a load_type record for a bits type.
/// `flags` is `F_LOAD_FXS_IS_BITS = 64` for direct leaves,
/// or `F_LOAD_FXS_IS_UNION = 4` when the bits type is a member of a union.
/// When the bits type is inside a union, `primitive` is `undefined` in the load_type.
fn push_bits_load_type(buf: &mut Vec<u8>, fields: &[(u32, &str)], size: u32, primitive: &str, flags: u32) {
    push_tuple(buf, 8); // load_type record = 8 elements
    push_atom(buf, "load_type");
    push_atom(buf, "undefined"); // name
    push_atom(buf, "undefined"); // base (bits has no XSD base)
    push_int(buf, flags as i64);
    push_atom(buf, "undefined"); // default_str
    push_atom(buf, "undefined"); // default
    if flags == 64 {
        push_atom(buf, primitive); // primitive only set for standalone bits
    } else {
        push_atom(buf, "undefined"); // undefined for union member
    }
    // data = #bits{fields, size}
    push_bits_derivation(buf, fields, size);
}

fn push_term(buf: &mut Vec<u8>, term: &Term) {
    match term {
        Term::Atom(a) => push_atom(buf, &a.name),
        Term::Tuple(t) => {
            push_tuple(buf, t.elements.len());
            for elem in &t.elements {
                push_term(buf, elem);
            }
        }
        Term::List(l) => {
            push_list_header(buf, l.elements.len());
            for elem in &l.elements {
                push_term(buf, elem);
            }
            if !l.elements.is_empty() {
                push_nil(buf);
            }
        }
        Term::FixInteger(i) => push_int(buf, i.value as i64),
        _ => panic!("unsupported term in union hash encoding: {term:?}"),
    }
}

fn push_union_exs_type(buf: &mut Vec<u8>, member_refs: &[Term]) {
    push_exs_type_header(buf, 2);
    push_tuple(buf, 2);
    push_atom(buf, "union");
    push_list_header(buf, member_refs.len());
    for member_ref in member_refs {
        push_term(buf, member_ref);
    }
    if !member_refs.is_empty() {
        push_nil(buf);
    }
    push_exs_type_tail(buf, 2);
}

fn push_union_load_type(buf: &mut Vec<u8>, member_refs: &[Term], flags: u32) {
    push_tuple(buf, 8);
    push_atom(buf, "load_type");
    push_atom(buf, "undefined");
    push_list_header(buf, member_refs.len());
    for member_ref in member_refs {
        push_term(buf, member_ref);
    }
    if !member_refs.is_empty() {
        push_nil(buf);
    }
    push_int(buf, flags as i64);
    push_atom(buf, "undefined"); // default_str
    push_atom(buf, "undefined"); // default
    push_atom(buf, "undefined"); // primitive (always undefined for union)
    push_atom(buf, "undefined"); // data
}

/// Compute phash2 of `{ExsType, LoadType, []}` for an inline bits type.
/// `flags` = `F_LOAD_FXS_IS_BITS = 64` for direct leaves,
/// `F_LOAD_FXS_IS_UNION = 4` for bits members inside a union.
fn compute_bits_thash(exs_bytes: &[u8], fields: &[(u32, &str)], size: u32, primitive: &str, flags: u32, misc: &[MiscEntry]) -> u32 {
    let mut lt_buf = Vec::new();
    push_bits_load_type(&mut lt_buf, fields, size, primitive, flags);
    compute_thash_with_load_type_misc(exs_bytes, &lt_buf, misc)
}

/// Build the eetf Term for a bits exs_type record.
fn build_bits_exs_type_eetf(name: &str, fields: &[(u32, &str)], size: u32) -> Term {
    let field_terms: Vec<Term> = fields
        .iter()
        .map(|(pos, n)| tuple(vec![int(*pos as i32), Term::from(eetf::ByteList::from(*n))]))
        .collect();
    let derivation = tuple(vec![atom("bits"), list(field_terms), int(size as i32)]);
    tuple(vec![
        atom("exs_type"),
        atom(name),
        atom("derived"),
        undefined(),
        undefined(),
        undefined(),
        undefined(),
        undefined(),
        derivation,
        nil(),
        undefined(),
        nil(),
        int(2),
    ])
}

/// Build the eetf Term for the load_type of an inline bits type.
/// `flags` = `F_LOAD_FXS_IS_BITS = 64` for direct leaves,
/// `F_LOAD_FXS_IS_UNION = 4` for bits inside a union.
fn build_bits_load_type_eetf(
    name: &str,
    fields: &[(u32, &str)],
    size: u32,
    primitive: &str,
    flags: u32,
) -> Term {
    let field_terms: Vec<Term> = fields
        .iter()
        .map(|(pos, n)| tuple(vec![int(*pos as i32), Term::from(eetf::ByteList::from(*n))]))
        .collect();
    let data = tuple(vec![atom("bits"), list(field_terms), int(size as i32)]);
    tuple(vec![
        atom("load_type"),
        atom(name),
        undefined(),
        int(flags as i32),
        undefined(),
        undefined(),
        if flags == 64 { atom(primitive) } else { undefined() },
        data,
    ])
}

fn build_union_exs_type_eetf(name: &str, member_refs: &[Term]) -> Term {
    tuple(vec![
        atom("exs_type"),
        atom(name),
        atom("derived"),
        undefined(),
        undefined(),
        undefined(),
        undefined(),
        undefined(),
        tuple(vec![atom("union"), list(member_refs.to_vec())]),
        nil(),
        undefined(),
        nil(),
        int(2),
    ])
}

fn build_union_load_type_eetf(name: &str, member_refs: &[Term], flags: u32) -> Term {
    tuple(vec![
        atom("load_type"),
        atom(name),
        list(member_refs.to_vec()),
        int(flags as i32),
        undefined(),
        undefined(),
        undefined(), // primitive is always undefined for union
        undefined(),
    ])
}

/// Compute the bits_type_size for a given max bit position:
/// returns 32 if max_pos < 32, 64 if max_pos < 64, else ((max_pos + 8) / 8) * 8.
pub fn bits_type_size(max_pos: u32) -> u32 {
    if max_pos < 32 {
        32
    } else if max_pos < 64 {
        64
    } else {
        ((max_pos + 8) / 8) * 8
    }
}

// ---------------------------------------------------------------------------
// Facet encoding helpers
// ---------------------------------------------------------------------------

/// Encode a range_facet to ETF bytes:
/// `{range_facet, [{min, max}], false, [], [], 0}`
pub fn encode_range_facet_bytes(ranges: &[(IntBound, IntBound)]) -> Vec<u8> {
    let mut buf = Vec::new();
    push_tuple(&mut buf, 6);
    push_atom(&mut buf, "range_facet");
    push_list_header(&mut buf, ranges.len());
    for (min, max) in ranges {
        if let (IntBound::Value(vmin, tmin), IntBound::Value(vmax, tmax)) = (min, max) {
            if vmin == vmax && tmin == tmax {
                // Exact value: encode as {single, {tag, N}} to match Erlang's mk_range.
                push_tuple(&mut buf, 2);
                push_atom(&mut buf, "single");
                push_int_bound(&mut buf, min);
                continue;
            }
        }
        push_tuple(&mut buf, 2);
        push_int_bound(&mut buf, min);
        push_int_bound(&mut buf, max);
    }
    if !ranges.is_empty() {
        push_nil(&mut buf);
    }
    push_atom(&mut buf, "false"); // fixed
    push_nil(&mut buf); // error_message
    push_nil(&mut buf); // error_app_tag
    push_int(&mut buf, 0); // step
    buf
}

/// Encode a length_facet to ETF bytes:
/// `{length, [{single, N} | {min, max}], false, [], [], value}`
/// Single-value ranges (min == max) are encoded as `{single, N}` to match Erlang's mk_range.
pub fn encode_length_facet_bytes(ranges: &[(u64, Option<u64>)]) -> Vec<u8> {
    let mut buf = Vec::new();
    push_tuple(&mut buf, 6);
    push_atom(&mut buf, "length");
    push_list_header(&mut buf, ranges.len());
    for &(min, max) in ranges {
        if max == Some(min) {
            // Exact value: encode as {single, N}
            push_tuple(&mut buf, 2);
            push_atom(&mut buf, "single");
            push_int(&mut buf, min as i64);
        } else {
            push_tuple(&mut buf, 2);
            push_int(&mut buf, min as i64);
            match max {
                Some(n) => push_int(&mut buf, n as i64),
                None => push_atom(&mut buf, "max"),
            }
        }
    }
    if !ranges.is_empty() {
        push_nil(&mut buf);
    }
    push_atom(&mut buf, "false"); // fixed
    push_nil(&mut buf); // error_message
    push_nil(&mut buf); // error_app_tag
    push_atom(&mut buf, "value"); // type
    buf
}

/// Encode an ignore_facet for an identityref base to ETF bytes:
/// `{ignore_facet, [{identity_base, {Ns, Name}}], false, [], []}`
pub fn encode_ignore_facet_bytes(ns: &str, name: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    push_tuple(&mut buf, 5);
    push_atom(&mut buf, "ignore_facet");
    // [{identity_base, {Ns, Name}}]
    push_list_header(&mut buf, 1);
    push_tuple(&mut buf, 2);
    push_atom(&mut buf, "identity_base");
    push_tuple(&mut buf, 2);
    push_atom(&mut buf, ns);
    push_atom(&mut buf, name);
    push_nil(&mut buf); // end of inner list
    push_atom(&mut buf, "false"); // fixed
    push_nil(&mut buf); // error_message
    push_nil(&mut buf); // error_app_tag
    buf
}

/// Build the eetf Term for an ignore_facet for an identityref base:
/// `{ignore_facet, [{identity_base, {Ns, Name}}], false, [], []}`
pub fn ignore_facet_eetf(ns: &str, name: &str) -> Term {
    tuple(vec![
        atom("ignore_facet"),
        list(vec![tuple(vec![
            atom("identity_base"),
            tuple(vec![atom(ns), atom(name)]),
        ])]),
        atom("false"),
        nil(),
        nil(),
    ])
}


/// `{fraction_digits, N, false, [], []}`
pub fn encode_fraction_digits_facet_bytes(n: u8) -> Vec<u8> {
    let mut buf = Vec::new();
    push_tuple(&mut buf, 5);
    push_atom(&mut buf, "fraction_digits");
    push_int(&mut buf, n as i64);
    push_atom(&mut buf, "false"); // fixed
    push_nil(&mut buf); // error_message
    push_nil(&mut buf); // error_app_tag
    buf
}

/// Build the eetf Term for a fraction_digits facet:
/// `{fraction_digits, N, false, [], []}`
pub fn fraction_digits_facet_eetf(n: u8) -> Term {
    tuple(vec![
        atom("fraction_digits"),
        int(n as i32),
        atom("false"),
        nil(),
        nil(),
    ])
}

/// Encode a pattern_facet to ETF bytes:
/// `{pattern, <<bytes>>, false, false, [], []}`
pub fn encode_pattern_facet_bytes(pattern: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    push_tuple(&mut buf, 6);
    push_atom(&mut buf, "pattern");
    push_binary(&mut buf, pattern); // NOTE: binary, not atom
    push_atom(&mut buf, "false"); // invert_match
    push_atom(&mut buf, "false"); // fixed
    push_nil(&mut buf); // error_message
    push_nil(&mut buf); // error_app_tag
    buf
}

// ---------------------------------------------------------------------------
// IntBound: integer range boundary value
// ---------------------------------------------------------------------------

/// A boundary value in a YANG integer range expression.
#[derive(Debug, Clone)]
pub enum IntBound {
    Value(i64, u8), // (value, xsd_type_tag from xsd.hrl)
    Min,            // "min" keyword
    Max,            // "max" keyword
}

fn push_int_bound(buf: &mut Vec<u8>, b: &IntBound) {
    match b {
        IntBound::Value(v, tag) => {
            push_tuple(buf, 2);
            push_int(buf, *tag as i64);
            push_int(buf, *v);
        }
        IntBound::Min => push_atom(buf, "min"),
        IntBound::Max => push_atom(buf, "max"),
    }
}

fn int_bound_eetf(b: &IntBound) -> Term {
    match b {
        IntBound::Value(v, tag) => tuple(vec![int(*tag as i32), int64(*v)]),
        IntBound::Min => atom("min"),
        IntBound::Max => atom("max"),
    }
}

// ---------------------------------------------------------------------------
// eetf Term builders (for writing to FXS file)
// ---------------------------------------------------------------------------

fn build_list_exs_type_eetf(name: &str, base_ns: &str, base_name: &str) -> Term {
    let derivation = tuple(vec![
        atom("list"),
        tuple(vec![atom(base_ns), atom(base_name)]),
    ]);
    tuple(vec![
        atom("exs_type"),
        atom(name),
        atom("derived"),
        undefined(),
        undefined(),
        undefined(),
        undefined(),
        undefined(),
        derivation,
        nil(),
        undefined(),
        nil(),
        int(19), // F_IS_LEAF_LIST | F_IS_GENERATED | F_IS_LEAF_LIST_AS_LIST
    ])
}

fn build_unique_list_exs_type_eetf(
    name: &str,
    list_ns: &str,
    list_name: &str,
    min: u64,
    max: Option<u64>,
) -> Term {
    let max_term = match max {
        Some(n) => int(n as i32),
        None => atom("unbounded"),
    };
    let unique_list = tuple(vec![
        atom("unique_list"),
        undefined(),
        atom("false"),
        nil(),
        nil(),
        int(min as i32),
        max_term,
    ]);
    let derivation = tuple(vec![
        atom("restriction"),
        tuple(vec![atom(list_ns), atom(list_name)]),
        list(vec![unique_list]),
    ]);
    tuple(vec![
        atom("exs_type"),
        atom(name),
        atom("derived"),
        undefined(),
        undefined(),
        undefined(),
        undefined(),
        undefined(),
        derivation,
        nil(),
        undefined(),
        nil(),
        int(2), // F_IS_GENERATED
    ])
}

fn build_restriction_exs_type_eetf(
    name: &str,
    base_ns: &str,
    base_name: &str,
    facets: Vec<Term>,
    flags: u32,
) -> Term {
    let derivation = tuple(vec![
        atom("restriction"),
        tuple(vec![atom(base_ns), atom(base_name)]),
        list(facets),
    ]);
    tuple(vec![
        atom("exs_type"),
        atom(name),
        atom("derived"),
        undefined(),
        undefined(),
        undefined(),
        undefined(),
        undefined(),
        derivation,
        nil(),
        undefined(),
        nil(),
        int(flags as i32),
    ])
}

// ---------------------------------------------------------------------------
// TypeGen: dedup state for t<hash> generated types
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct GenEntry {
    name: String,
    exs_term: Term,
    load_type_term: Option<Term>,
}

/// Build FXS doc/code_name records for a newly created type's Misc entries.
/// Mirrors Erlang's `insert_misc` which transforms `{enum/bit, Name}` to `{doc, {enum/bit, TypeAtom, Name}}`.
///
/// `type_name` = just the type name atom string (e.g., "t1389886348"), NOT the `{Ns, TypeAtom}` tuple.
/// In yanger_fxs, `insert_misc(Misc, TypeName, ...)` is called with `{_Ns, TypeName} = NsT`.
/// Returns records in the order they should be pushed to doc_misc_records (forward DFS order).
fn build_type_doc_terms(type_name: &str, misc: &[MiscEntry]) -> Vec<Term> {
    use crate::terms::charlist;
    let mut records = Vec::new();
    for entry in misc {
        match entry {
            MiscEntry::EnumDoc { name, desc, flags } => {
                // {doc, {doc, {enum, TypeAtom, <<name>>}}, <<desc>>, flags, undefined}
                let doc_name = tuple(vec![
                    atom("doc"),
                    tuple(vec![
                        atom("enum"),
                        atom(type_name),
                        binary(name.clone()),
                    ]),
                ]);
                records.push(tuple(vec![
                    atom("doc"),
                    doc_name,
                    binary(desc.clone()),
                    int(*flags as i32),
                    undefined(),
                ]));
            }
            MiscEntry::BitDoc { name, desc, flags } => {
                // {doc, {doc, {bit, TypeAtom, CharList(name)}}, <<desc>>, flags, undefined}
                // Bit names use STRING_EXT (Erlang char list) per yanger's `?b2l(BinName)`
                let name_str = std::str::from_utf8(name).unwrap_or("");
                let doc_name = tuple(vec![
                    atom("doc"),
                    tuple(vec![
                        atom("bit"),
                        atom(type_name),
                        charlist(name_str),
                    ]),
                ]);
                records.push(tuple(vec![
                    atom("doc"),
                    doc_name,
                    binary(desc.clone()),
                    int(*flags as i32),
                    undefined(),
                ]));
            }
            MiscEntry::BitCodeName { name, code_name } => {
                // {code_name, {code_name, TypeAtom, <<name>>}, <<code_name>>}
                let cn_name = tuple(vec![
                    atom("code_name"),
                    atom(type_name),
                    binary(name.clone()),
                ]);
                records.push(tuple(vec![
                    atom("code_name"),
                    cn_name,
                    binary(code_name.clone()),
                ]));
            }
        }
    }
    records
}

/// Tracks all generated t<hash> types for a module.
///
/// Mirrors yanger_fxs's `GenTypes` dict with deduplication by hash
/// and collision handling (hash+1 on collision).
pub struct TypeGen {
    by_hash: HashMap<u32, GenEntry>,
    by_name: HashMap<String, bool>,
    entries: Vec<GenEntry>,
    /// Doc entries for enum/bit values with tailf:info, accumulated in forward DFS order.
    /// These are written to the FXS Misc2 section (before the module description).
    pub doc_misc_records: Vec<Term>,
}

impl TypeGen {
    pub fn new() -> Self {
        TypeGen {
            by_hash: HashMap::new(),
            by_name: HashMap::new(),
            entries: Vec::new(),
            doc_misc_records: Vec::new(),
        }
    }

    fn intern(
        &mut self,
        mut hash: u32,
        build_eetf: impl Fn(&str) -> Term,
        build_load_type: Option<impl Fn(&str) -> Term>,
    ) -> String {
        if let Some(e) = self.by_hash.get(&hash) {
            return e.name.clone();
        }
        let name = loop {
            let candidate = format!("t{hash}");
            if !self.by_name.contains_key(&candidate) {
                break candidate;
            }
            hash = hash.wrapping_add(1);
        };
        let exs_term = build_eetf(&name);
        let load_type_term = build_load_type.map(|f| f(&name));
        let entry = GenEntry {
            name: name.clone(),
            exs_term,
            load_type_term,
        };
        self.by_hash.insert(hash, entry.clone());
        self.by_name.insert(name.clone(), true);
        self.entries.push(entry);
        name
    }

    /// Get or create the list base type for a leaf-list.
    /// Returns `{module_ns, type_name}` as an eetf Term.
    ///
    /// When `all_load_type_flags != 0` (e.g. IS_IDENTITY_DERIVATION=65536 for yang:phys-address,
    /// or IS_UNION=4 for inet:ip-address), the load_type is included in the hash computation and
    /// a load_type record is generated — mirroring yanger_fxs's `mk_exs_type/7` behaviour.
    pub fn get_or_create_list_type(
        &mut self,
        module_ns: &str,
        base_ns: &str,
        base_name: &str,
        all_load_type_flags: u32,
    ) -> Term {
        let mut exs_bytes = Vec::new();
        push_list_exs_type(&mut exs_bytes, base_ns, base_name);
        let hash = if all_load_type_flags != 0 {
            let mut lt_buf = Vec::new();
            push_simple_load_type(&mut lt_buf, base_ns, base_name, all_load_type_flags);
            compute_thash_with_load_type(&exs_bytes, &lt_buf)
        } else {
            compute_thash_from_exs_bytes(&exs_bytes)
        };
        let base_ns = base_ns.to_owned();
        let base_name = base_name.to_owned();
        let name = if all_load_type_flags != 0 {
            let bn = base_ns.clone();
            let bname = base_name.clone();
            let flags = all_load_type_flags;
            self.intern(
                hash,
                move |n| build_list_exs_type_eetf(n, &base_ns, &base_name),
                Some(move |n: &str| build_simple_load_type_eetf(n, &bn, &bname, flags)),
            )
        } else {
            self.intern(
                hash,
                move |n| build_list_exs_type_eetf(n, &base_ns, &base_name),
                None::<fn(&str) -> Term>,
            )
        };
        tuple(vec![atom(module_ns), atom(&name)])
    }

    /// Get or create the unique_list restriction type for a leaf-list.
    /// Returns `{module_ns, type_name}` as an eetf Term.
    ///
    /// When `all_load_type_flags != 0`, the load_type is included in hash computation
    /// and a load_type record is emitted, mirroring yanger_fxs's `mk_exs_type/7`.
    pub fn get_or_create_unique_list_type(
        &mut self,
        module_ns: &str,
        list_ns: &str,
        list_name: &str,
        min: u64,
        max: Option<u64>,
        all_load_type_flags: u32,
    ) -> Term {
        let mut exs_bytes = Vec::new();
        push_unique_list_exs_type(&mut exs_bytes, list_ns, list_name, min, max);
        let hash = if all_load_type_flags != 0 {
            let mut lt_buf = Vec::new();
            push_simple_load_type(&mut lt_buf, list_ns, list_name, all_load_type_flags);
            compute_thash_with_load_type(&exs_bytes, &lt_buf)
        } else {
            compute_thash_from_exs_bytes(&exs_bytes)
        };
        let list_ns = list_ns.to_owned();
        let list_name = list_name.to_owned();
        let name = if all_load_type_flags != 0 {
            let ln = list_ns.clone();
            let lname = list_name.clone();
            let flags = all_load_type_flags;
            self.intern(
                hash,
                move |n| build_unique_list_exs_type_eetf(n, &list_ns, &list_name, min, max),
                Some(move |n: &str| build_simple_load_type_eetf(n, &ln, &lname, flags)),
            )
        } else {
            self.intern(
                hash,
                move |n| build_unique_list_exs_type_eetf(n, &list_ns, &list_name, min, max),
                None::<fn(&str) -> Term>,
            )
        };
        tuple(vec![atom(module_ns), atom(&name)])
    }

    /// Get or create a string/integer restriction type for inline constraints.
    /// `facet_bytes`: ETF-encoded facets (from `encode_*_facet_bytes`).
    /// `facets_eetf`: eetf Terms for the same facets.
    /// `flags`: ExsType flags (always 2 = F_EXS_TYPE_IS_GENERATED_BY_YANGER for restriction types).
    /// `load_type_flags`: LoadTypeFlags from context (0 for direct leaves, IS_UNION/GET_TYPE_INFO for union members).
    ///   When non-zero, the load_type `{base_ns, base_name}` with these flags is included in the hash.
    /// Returns `{module_ns, type_name}` as an eetf Term.
    pub fn get_or_create_restriction_type(
        &mut self,
        module_ns: &str,
        base_ns: &str,
        base_name: &str,
        facet_bytes: Vec<Vec<u8>>,
        facets_eetf: Vec<Term>,
        flags: u32,
    ) -> Term {
        self.get_or_create_restriction_type_with_load_flags(
            module_ns, base_ns, base_name, facet_bytes, facets_eetf, flags, 0,
        )
    }

    /// Like `get_or_create_restriction_type` but includes load_type in hash when `load_type_flags != 0`.
    /// Used when the restriction type is a member of a union or other context that uses a load_type.
    pub fn get_or_create_restriction_type_with_load_flags(
        &mut self,
        module_ns: &str,
        base_ns: &str,
        base_name: &str,
        facet_bytes: Vec<Vec<u8>>,
        facets_eetf: Vec<Term>,
        flags: u32,
        load_type_flags: u32,
    ) -> Term {
        let mut exs_bytes = Vec::new();
        push_restriction_exs_type(&mut exs_bytes, base_ns, base_name, &facet_bytes, flags);
        let hash = if load_type_flags != 0 {
            let mut lt_buf = Vec::new();
            push_simple_load_type(&mut lt_buf, base_ns, base_name, load_type_flags);
            compute_thash_with_load_type(&exs_bytes, &lt_buf)
        } else {
            compute_thash_from_exs_bytes(&exs_bytes)
        };
        let base_ns = base_ns.to_owned();
        let base_name = base_name.to_owned();
        let load_type_flags_copy = load_type_flags;
        let name = if load_type_flags != 0 {
            let bns = base_ns.clone();
            let bname = base_name.clone();
            self.intern(
                hash,
                move |n| {
                    build_restriction_exs_type_eetf(n, &base_ns, &base_name, facets_eetf.clone(), flags)
                },
                Some(move |n: &str| build_simple_load_type_eetf(n, &bns, &bname, load_type_flags_copy)),
            )
        } else {
            self.intern(
                hash,
                move |n| {
                    build_restriction_exs_type_eetf(n, &base_ns, &base_name, facets_eetf.clone(), flags)
                },
                None::<fn(&str) -> Term>,
            )
        };
        tuple(vec![atom(module_ns), atom(&name)])
    }

    /// Get or create an inline enumeration type.
    ///
    /// `facets`: (name_bytes, code_name_bytes, value) tuples in **REVERSE YANG declaration order**
    /// (to match yanger's `foldl` prepend behaviour).
    /// Returns `{module_ns, type_name}` as an eetf Term.
    ///
    /// Yanger processes grouping definitions first with `LoadTypeFlags=0` (→ `LoadType=undefined`),
    /// so inline enums in groupings are registered without a load_type (hash uses `undefined`).
    /// For direct non-grouping inline enums, yanger uses IS_ENUMERATION in the hash with a load_type.
    ///
    /// To replicate this two-phase behaviour, the FXS plugin pre-registers enum types found in
    /// grouping definitions (via `pre_register_enum_type_no_load`) before walking the schema tree.
    /// This method then:
    ///  1. For `flags = 0` (typedef union member, `LoadTypeFlags=0→LoadType=undefined`):
    ///     uses no-load hash (same as pre-registered), registers with no load_type.
    ///  2. For `flags = IS_ENUMERATION (2)`: checks if the type was pre-registered
    ///     (undefined hash, no load_type); if so, returns it. Otherwise registers with
    ///     IS_ENUMERATION hash + load_type (non-grouping direct leaf).
    ///  3. For `flags = IS_UNION (4)`: always registers/returns with the IS_UNION hash,
    ///     bypassing the pre-registered check (enum members of unions get a different type name).
    pub fn get_or_create_enum_type(
        &mut self,
        module_ns: &str,
        facets: &[(Vec<u8>, Option<Vec<u8>>, i64)],
        flags: u32,
        misc: &[MiscEntry],
    ) -> Term {
        let mut exs_bytes = Vec::new();
        push_enum_exs_type(&mut exs_bytes, facets);

        if flags == 0 || flags == 2 {
            // No-load hash check: covers both pre-registered (grouping) and flags=0 (typedef union member).
            let no_lt_hash = compute_thash_from_exs_bytes_misc(&exs_bytes, misc);
            if let Some(e) = self.by_hash.get(&no_lt_hash) {
                return tuple(vec![atom(module_ns), atom(&e.name.clone())]);
            }

            if flags == 0 {
                // LoadTypeFlags=0 → LoadType=undefined: register with no-load hash, no load_type.
                let facets_owned: Vec<(Vec<u8>, Option<Vec<u8>>, i64)> = facets.to_vec();
                let name = self.intern(
                    no_lt_hash,
                    move |n| build_enum_exs_type_eetf(n, &facets_owned),
                    None::<fn(&str) -> Term>,
                );
                if !misc.is_empty() {
                    
                    self.doc_misc_records.extend(build_type_doc_terms(&name, misc));
                }
                return tuple(vec![atom(module_ns), atom(&name)]);
            }
        }

        // flags=2 (not pre-registered) or flags=4 (IS_UNION): register with flags hash + load_type.
        let hash = compute_enum_thash(&exs_bytes, facets, flags, misc);
        let is_new = !self.by_hash.contains_key(&hash);
        let facets_owned: Vec<(Vec<u8>, Option<Vec<u8>>, i64)> = facets.to_vec();
        let facets_for_lt = facets.to_vec();
        let name = self.intern(
            hash,
            move |n| build_enum_exs_type_eetf(n, &facets_owned),
            Some(move |n: &str| build_enum_load_type_eetf(n, &facets_for_lt, flags)),
        );
        if is_new && !misc.is_empty() {
            
            self.doc_misc_records.extend(build_type_doc_terms(&name, misc));
        }
        tuple(vec![atom(module_ns), atom(&name)])
    }

    /// Pre-register an inline enum type from a grouping definition, using `undefined` as LoadType
    /// in the hash (matching yanger's `add_enumeration_types` behaviour with `LoadTypeFlags=0`).
    /// No load_type record is generated — only the exs_type.
    pub fn pre_register_enum_type_no_load(
        &mut self,
        module_ns: &str,
        facets: &[(Vec<u8>, Option<Vec<u8>>, i64)],
        misc: &[MiscEntry],
    ) {
        if facets.is_empty() {
            return;
        }
        let mut exs_bytes = Vec::new();
        push_enum_exs_type(&mut exs_bytes, facets);
        // Skip if already registered under the undefined hash (e.g., duplicate grouping scan).
        let no_lt_hash = compute_thash_from_exs_bytes_misc(&exs_bytes, misc);
        if self.by_hash.contains_key(&no_lt_hash) {
            return;
        }
        // Skip if already registered under the IS_ENUM or IS_UNION hash — this means the enum was
        // encountered in the schema tree walk (collect_types_forward) already.
        let is_enum_hash = compute_enum_thash(&exs_bytes, facets, 2, misc);
        if self.by_hash.contains_key(&is_enum_hash) {
            return;
        }
        let is_union_hash = compute_enum_thash(&exs_bytes, facets, 4, misc);
        if self.by_hash.contains_key(&is_union_hash) {
            return;
        }
        // Not yet registered: this is an enum from an UNUSED grouping (never expanded into
        // the module's schema tree). Register with undefined hash and no load_type.
        let facets_owned = facets.to_vec();
        let name = self.intern(
            no_lt_hash,
            move |n| build_enum_exs_type_eetf(n, &facets_owned),
            None::<fn(&str) -> Term>,
        );
        if !misc.is_empty() {
            
            self.doc_misc_records.extend(build_type_doc_terms(&name, misc));
        }
    }

    /// Get or create an inline bits type.
    ///
    /// `fields`: `(position, name)` pairs in YANG declaration order.
    /// Returns `{module_ns, type_name}` as an eetf Term.
    pub fn get_or_create_bits_type(
        &mut self,
        module_ns: &str,
        fields: Vec<(u32, String)>,
        size: u32,
        flags: u32,
        misc: &[MiscEntry],
    ) -> Term {
        let max_pos = fields.iter().map(|(p, _)| *p).max().unwrap_or(0);
        let fields_ref: Vec<(u32, &str)> = fields.iter().map(|(p, n)| (*p, n.as_str())).collect();
        let mut exs_bytes = Vec::new();
        push_bits_exs_type(&mut exs_bytes, &fields_ref, size);
        let primitive = bits_primitive_atom(max_pos).to_owned();

        if flags == 0 {
            // LoadTypeFlags=0 → LoadType=undefined: register with no-load hash, no load_type.
            let no_lt_hash = compute_thash_from_exs_bytes_misc(&exs_bytes, misc);
            if let Some(e) = self.by_hash.get(&no_lt_hash) {
                return tuple(vec![atom(module_ns), atom(&e.name.clone())]);
            }
            let fields_for_exs = fields.clone();
            let name = self.intern(
                no_lt_hash,
                move |n| {
                    let refs: Vec<(u32, &str)> = fields_for_exs
                        .iter()
                        .map(|(p, s)| (*p, s.as_str()))
                        .collect();
                    build_bits_exs_type_eetf(n, &refs, size)
                },
                None::<fn(&str) -> Term>,
            );
            if !misc.is_empty() {
                
                self.doc_misc_records.extend(build_type_doc_terms(&name, misc));
            }
            return tuple(vec![atom(module_ns), atom(&name)]);
        }

        let hash = compute_bits_thash(&exs_bytes, &fields_ref, size, &primitive, flags, misc);
        let is_new = !self.by_hash.contains_key(&hash);
        let fields_for_exs = fields.clone();
        let fields_for_lt = fields.clone();
        let prim_for_lt = primitive.clone();
        let name = self.intern(
            hash,
            move |n| {
                let refs: Vec<(u32, &str)> = fields_for_exs
                    .iter()
                    .map(|(p, s)| (*p, s.as_str()))
                    .collect();
                build_bits_exs_type_eetf(n, &refs, size)
            },
            Some(move |n: &str| {
                let refs: Vec<(u32, &str)> = fields_for_lt
                    .iter()
                    .map(|(p, s)| (*p, s.as_str()))
                    .collect();
                build_bits_load_type_eetf(n, &refs, size, &prim_for_lt, flags)
            }),
        );
        if is_new && !misc.is_empty() {
            
            self.doc_misc_records.extend(build_type_doc_terms(&name, misc));
        }
        tuple(vec![atom(module_ns), atom(&name)])
    }

    /// Generate an inline union type: `{module_ns, t<hash>}`.
    /// `load_type_flags`: flags stored in the load_type record, affects the hash.
    ///   - `IS_UNION = 4` for unions with all-builtin or same-module-typedef members
    ///   - `GET_TYPE_INFO | GET_DEFAULT = 1152` for cross-module typedef unions (non-mandatory leaf)
    ///   - `GET_TYPE_INFO = 1024` for cross-module typedef unions (mandatory/key leaf)
    pub fn get_or_create_union_type(
        &mut self,
        module_ns: &str,
        member_refs: Vec<Term>,
        load_type_flags: u32,
    ) -> Term {
        let mut exs_bytes = Vec::new();
        push_union_exs_type(&mut exs_bytes, &member_refs);
        let mut lt_buf = Vec::new();
        push_union_load_type(&mut lt_buf, &member_refs, load_type_flags);
        let hash = compute_thash_with_load_type(&exs_bytes, &lt_buf);
        let refs_for_exs = member_refs.clone();
        let refs_for_lt = member_refs.clone();
        let lt_flags = load_type_flags;
        let name = self.intern(
            hash,
            move |n| build_union_exs_type_eetf(n, &refs_for_exs),
            Some(move |n: &str| build_union_load_type_eetf(n, &refs_for_lt, lt_flags)),
        );
        tuple(vec![atom(module_ns), atom(&name)])
    }

    /// Return the current number of registered type entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return all generated exs_type Terms in generation order.
    pub fn exs_type_terms(&self) -> Vec<Term> {
        self.entries.iter().map(|e| e.exs_term.clone()).collect()
    }

    /// Return exs_type Terms for entries in the range [start, end).
    pub fn exs_type_terms_range(&self, start: usize, end: usize) -> Vec<Term> {
        self.entries[start..end].iter().map(|e| e.exs_term.clone()).collect()
    }

    /// Return exs_type Terms for entries from `start` to end.
    pub fn exs_type_terms_from(&self, start: usize) -> Vec<Term> {
        self.entries[start..].iter().map(|e| e.exs_term.clone()).collect()
    }

    /// Return all load_type Terms for enum (and future bits/union) inline types.
    pub fn load_type_terms(&self) -> Vec<Term> {
        self.entries
            .iter()
            .filter_map(|e| e.load_type_term.clone())
            .collect()
    }

    /// Return load_type Terms for entries in the range [start, end).
    pub fn load_type_terms_range(&self, start: usize, end: usize) -> Vec<Term> {
        self.entries[start..end]
            .iter()
            .filter_map(|e| e.load_type_term.clone())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// YANG type analysis helpers
// ---------------------------------------------------------------------------

/// Map YANG integer type name to XSD type name and type tag.
pub fn yang_int_to_xsd_info(yang_type: &str) -> Option<(&'static str, u8)> {
    match yang_type {
        "int8" => Some(("byte", 6)),
        "int16" => Some(("short", 7)),
        "int32" => Some(("int", 8)),
        "int64" => Some(("long", 9)),
        "uint8" => Some(("unsignedByte", 10)),
        "uint16" => Some(("unsignedShort", 11)),
        "uint32" => Some(("unsignedInt", 12)),
        "uint64" => Some(("unsignedLong", 13)),
        _ => None,
    }
}

/// Parse a YANG `length` statement arg like `"0..256"`, `"1..max"`, `"1"`.
pub fn parse_length_ranges(arg: &str) -> Vec<(u64, Option<u64>)> {
    let mut result = Vec::new();
    for part in arg.split('|') {
        let part = part.trim();
        if let Some(pos) = part.find("..") {
            let min_s = part[..pos].trim();
            let max_s = part[pos + 2..].trim();
            let min: u64 = if min_s == "min" {
                0
            } else {
                min_s.parse().unwrap_or(0)
            };
            let max: Option<u64> = if max_s == "max" {
                None
            } else {
                Some(max_s.parse().unwrap_or(u64::MAX))
            };
            result.push((min, max));
        } else {
            let v: u64 = part.parse().unwrap_or(0);
            result.push((v, Some(v)));
        }
    }
    result
}

/// Parse a YANG `range` statement arg like `"0..65535"`, `"min..max"`.
/// `tag` is the XSD type tag for the integer type.
pub fn parse_range_bounds(arg: &str, tag: u8) -> Vec<(IntBound, IntBound)> {
    let mut result = Vec::new();
    for part in arg.split('|') {
        let part = part.trim();
        if let Some(pos) = part.find("..") {
            let min_s = part[..pos].trim();
            let max_s = part[pos + 2..].trim();
            result.push((parse_bound(min_s, tag), parse_bound(max_s, tag)));
        } else {
            let v = parse_bound(part, tag);
            result.push((v.clone(), v));
        }
    }
    result
}

fn parse_bound(s: &str, tag: u8) -> IntBound {
    match s {
        "min" => IntBound::Min,
        "max" => IntBound::Max,
        _ => IntBound::Value(s.parse().unwrap_or(0), tag),
    }
}

/// Build the eetf Term for a range_facet (for writing to FXS).
pub fn range_facet_eetf(ranges: &[(IntBound, IntBound)]) -> Term {
    let pairs: Vec<Term> = ranges
        .iter()
        .map(|(min, max)| {
            if let (IntBound::Value(vmin, tmin), IntBound::Value(vmax, tmax)) = (min, max) {
                if vmin == vmax && tmin == tmax {
                    // Exact value: encode as {single, {tag, N}} to match Erlang's mk_range.
                    return tuple(vec![atom("single"), int_bound_eetf(min)]);
                }
            }
            tuple(vec![int_bound_eetf(min), int_bound_eetf(max)])
        })
        .collect();
    tuple(vec![
        atom("range_facet"),
        list(pairs),
        atom("false"),
        nil(),
        nil(),
        int(0),
    ])
}

/// Build the eetf Term for a length_facet (for writing to FXS).
pub fn length_facet_eetf(ranges: &[(u64, Option<u64>)]) -> Term {
    let pairs: Vec<Term> = ranges
        .iter()
        .map(|&(min, max)| {
            // Exact value (no range): emit ('single', N) instead of (N, N)
            if max == Some(min) {
                tuple(vec![atom("single"), int(min as i32)])
            } else {
                tuple(vec![
                    int(min as i32),
                    match max {
                        Some(n) => int(n as i32),
                        None => atom("max"),
                    },
                ])
            }
        })
        .collect();
    tuple(vec![
        atom("length"),
        list(pairs),
        atom("false"),
        nil(),
        nil(),
        atom("value"),
    ])
}

/// Build the eetf Term for a pattern_facet (for writing to FXS).
pub fn pattern_facet_eetf(pattern: &[u8]) -> Term {
    tuple(vec![
        atom("pattern"),
        Term::from(eetf::Binary {
            bytes: pattern.to_vec(),
        }),
        atom("false"),
        atom("false"),
        nil(),
        nil(),
    ])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema";
    const AAA_NS: &str = "http://cisco.com/ns/yang/Cisco-IOS-XE-aaa-oper";

    #[test]
    fn test_list_type_hash() {
        // Verified: erlang:phash2 in OTP 29 gives 1243695002 for list/string type
        let mut tgen = TypeGen::new();
        tgen.get_or_create_list_type(AAA_NS, XSD_NS, "string", 0);
        let entry = tgen.entries.first().unwrap();
        assert_eq!(entry.name, "t1243695002", "list hash: got {}", entry.name);
    }

    #[test]
    fn test_unique_list_type_hash() {
        // Verified: unique_list restriction on t1243695002, min=0, max=unbounded → 1227462091
        let mut tgen = TypeGen::new();
        // First create the list type
        let list_ref = tgen.get_or_create_list_type(AAA_NS, XSD_NS, "string", 0);
        let list_name = if let Term::Tuple(t) = &list_ref {
            if let Term::Atom(a) = &t.elements[1] {
                a.name.clone()
            } else {
                panic!()
            }
        } else {
            panic!()
        };
        // Then create the unique_list type
        tgen.get_or_create_unique_list_type(AAA_NS, AAA_NS, &list_name, 0, None, 0);
        let entry = tgen.entries.get(1).unwrap();
        assert_eq!(
            entry.name, "t1227462091",
            "unique_list hash: got {}",
            entry.name
        );
    }

    #[test]
    fn test_range_facet_hash() {
        // Verified: range 1024..65535 on unsignedShort (tag=11) → t1222396681
        let ranges = vec![(IntBound::Value(1024, 11), IntBound::Value(65535, 11))];
        let facet_bytes = vec![encode_range_facet_bytes(&ranges)];
        let facets_eetf = vec![range_facet_eetf(&ranges)];
        let mut tgen = TypeGen::new();
        tgen.get_or_create_restriction_type(
            "some-ns",
            XSD_NS,
            "unsignedShort",
            facet_bytes,
            facets_eetf,
            2,
        );
        let entry = tgen.entries.first().unwrap();
        assert_eq!(
            entry.name, "t1222396681",
            "range_facet hash: got {}",
            entry.name
        );
    }

    #[test]
    fn test_enum_type_hash() {
        // Verified in Erlang: openconfig-mpls-sr inline enum {ADJ_SID_ONLY=0, MIXED_MODE=1}
        // foldl reverses: facets = [MIXED_MODE=1, ADJ_SID_ONLY=0]
        // phash2({ExsType, undefined, []}) = 3283592721 (no-load hash)
        // phash2({ExsType, LT{flags=2}, []}) = 2091805284 (IS_ENUM hash, for direct leaves)
        let facets = vec![
            (b"MIXED_MODE".to_vec(), None, 1i64),
            (b"ADJ_SID_ONLY".to_vec(), None, 0i64),
        ];
        let mut tgen = TypeGen::new();
        let ns = "http://openconfig.net/yang/mpls";
        tgen.get_or_create_enum_type(ns, &facets, 2, &[]);
        let entry = tgen.entries.first().unwrap();
        assert_eq!(entry.name, "t2091805284", "enum hash: got {}", entry.name);
    }
}
