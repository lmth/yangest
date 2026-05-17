//! Fast FXS file printer.
//!
//! Reads one or more `.fxs` files and prints their contents in the same
//! human-readable format as `confdc --print-fxs`, but entirely in Rust —
//! no Erlang runtime required.
//!
//! Usage: fxs-print FILE [FILE...]

use std::io::{self, BufWriter, Cursor, Write};
use std::path::Path;

use eetf::Term;
use flate2::read::ZlibDecoder;

// ---------------------------------------------------------------------------
// FXS binary parser
// ---------------------------------------------------------------------------

const FXS_MAGIC: [u8; 4] = [0x04, 0x07, 0x06, 0x08];

struct FxsFile {
    header: Term,
    sections: Vec<Vec<Term>>,
}

fn parse_fxs(data: &[u8]) -> Result<FxsFile, String> {
    let mut pos = 0;

    // Magic
    if data.len() < 4 || &data[..4] != &FXS_MAGIC {
        return Err("not an FXS file (bad magic)".into());
    }
    pos += 4;

    // Header: [Sz:32][ETF uncompressed]
    let header = read_uncompressed_term(data, &mut pos)?;

    // Data sections: read until we hit a 0-length chunk or run out of data.
    let mut sections: Vec<Vec<Term>> = Vec::new();
    loop {
        if pos + 4 > data.len() {
            break;
        }
        let sz = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        if sz == 0 {
            break;
        }
        if pos + sz > data.len() {
            return Err(format!("section truncated at pos {pos}: need {sz} bytes"));
        }
        let chunk = &data[pos..pos + sz];
        pos += sz;

        let term = decode_chunk(chunk)?;
        // Each chunk is an Erlang list whose elements are the actual records.
        let items = collect_list(&term);
        sections.push(items);
    }

    Ok(FxsFile { header, sections })
}

/// Decode a chunk: either plain ETF or zlib-compressed ETF (Erlang's term_to_binary compressed).
fn decode_chunk(data: &[u8]) -> Result<Term, String> {
    if data.len() >= 2 && data[0] == 131 && data[1] == 80 {
        // Compressed term: [131, 80, u32be(uncompressed_len), zlib_data...]
        if data.len() < 6 {
            return Err("compressed term too short".into());
        }
        let uncomp_len = u32::from_be_bytes(data[2..6].try_into().unwrap()) as usize;
        let zlib_data = &data[6..];
        let mut dec = ZlibDecoder::new(zlib_data);
        let mut uncompressed = Vec::with_capacity(uncomp_len + 1);
        uncompressed.push(131u8); // ETF tag
        io::copy(&mut dec, &mut uncompressed).map_err(|e| format!("zlib decode: {e}"))?;
        Term::decode(Cursor::new(&uncompressed)).map_err(|e| format!("ETF decode: {e}"))
    } else {
        Term::decode(Cursor::new(data)).map_err(|e| format!("ETF decode: {e}"))
    }
}

fn read_uncompressed_term(data: &[u8], pos: &mut usize) -> Result<Term, String> {
    if *pos + 4 > data.len() {
        return Err("truncated: no room for size".into());
    }
    let sz = u32::from_be_bytes(data[*pos..*pos + 4].try_into().unwrap()) as usize;
    *pos += 4;
    if *pos + sz > data.len() {
        return Err(format!("truncated: need {sz} bytes at {pos}"));
    }
    let slice = &data[*pos..*pos + sz];
    *pos += sz;
    Term::decode(Cursor::new(slice)).map_err(|e| format!("ETF decode: {e}"))
}

/// Flatten an Erlang list term into a Vec<Term>.
/// fxs_write_list writes chunks of ≤256 items each reversed, so we need to
/// reverse each chunk to restore declaration order.
fn empty_list() -> Term {
    Term::List(eetf::List { elements: vec![] })
}

fn collect_list(term: &Term) -> Vec<Term> {
    match term {
        Term::List(list) if list.elements.is_empty() => Vec::new(),
        Term::List(list) => {
            let mut items: Vec<Term> = list.elements.clone();
            items.reverse(); // undo the fxs_write_list reversal
            items
        }
        _ => vec![term.clone()],
    }
}

// ---------------------------------------------------------------------------
// Printer
// ---------------------------------------------------------------------------

fn print_fxs(file: &FxsFile, out: &mut dyn Write) {
    // The header is {FXS_VSN, fxs_header_tuple}
    let fxs_header = match &file.header {
        Term::Tuple(t) if t.elements.len() == 2 => &t.elements[1],
        _ => &file.header,
    };
    print_record_term(fxs_header, false, out);

    // Sections in emission order (from serial.rs):
    //   0: ExsTypes, 1: LoadTypes, 2: AugL (load_augments),
    //   3: CsCdbL, 4: Identities, 5: CsL, 6: Misc2,
    //   7: HashDict, 8: CallpointInfo
    for section in &file.sections {
        for record in section {
            print_record_term(record, true, out);
        }
    }
}

fn print_record_term(term: &Term, blank_before: bool, out: &mut dyn Write) {
    let tup = match term {
        Term::Tuple(t) => t,
        _ => {
            writeln!(out, "{}", format_term(term)).unwrap();
            return;
        }
    };
    if tup.elements.is_empty() {
        return;
    }
    let tag = match &tup.elements[0] {
        Term::Atom(a) => a.name.as_str(),
        _ => {
            writeln!(out, "{}", format_term(term)).unwrap();
            return;
        }
    };

    let fields = record_fields(tag);
    if fields.is_empty() {
        // Unknown record — just print as a raw tuple.
        writeln!(out, "{}", format_term(term)).unwrap();
        return;
    }

    let elems = &tup.elements[1..]; // skip record tag
    if blank_before {
        writeln!(out).unwrap();
    }
    writeln!(out, "record {tag} {{").unwrap();
    let n = fields.len();
    for (i, fname) in fields.iter().enumerate() {
        let val = elems.get(i).map_or_else(empty_list, |t| t.clone());
        let formatted = format_field(tag, fname, &val, i);
        let comma = if i + 1 < n { "," } else { "" };
        writeln!(out, "     {fname} = {formatted}{comma}").unwrap();
    }
    writeln!(out, "     }}").unwrap();
}

/// Return the list of field names for a known record type.
fn record_fields(tag: &str) -> &'static [&'static str] {
    match tag {
        "fxs_header" => &[
            "key",
            "id",
            "uri",
            "type",
            "exported_agents",
            "prefix",
            "id_hash_value",
            "ns_dependencies",
            "checksum",
            "flags",
            "mountpoint",
            "tid",
            "snmp_info",
            "yang_header",
            "merged_revisions",
            "model_sizes",
            "cdb_checksum",
            "augments",
            "sections",
            "mount_id",
            "xmlns",
            "ns_to_prefix_maps",
        ],
        "yang_header" => &[
            "yang_version",
            "revision",
            "features",
            "deviations",
            "module_name",
            "flags",
            "submodules",
            "imports",
        ],
        "cs" => &[
            "tagpath",
            "htag",
            "namespace",
            "hnamespace",
            "exs",
            "keys",
            "flags",
            "dbm",
            "dba",
            "validatemfas",
            "actions",
            "cmp",
            "hooks",
            "hidden",
            "notifs",
            "symlink",
            "extra",
            "default_ref",
            "secondary_indices",
            "cli_flags",
            "oper_dbm",
            "oper_dba",
            "structures",
        ],
        "exs" => &[
            "tagpath",
            "type",
            "primitive_type",
            "default",
            "attrs",
            "min_occurs",
            "max_occurs",
            "children",
            "flags",
            "extra",
        ],
        "hash" => &["name", "type", "hash_value", "flags", "code_name"],
        "exs_type" => &[
            "name",
            "type",
            "lexical_value_fun",
            "value_fun",
            "value2value_fun",
            "string_fun",
            "string_cli_fun",
            "derivation",
            "desc",
            "check_value",
            "extra",
            "flags",
        ],
        "load_type" => &[
            "name",
            "base",
            "flags",
            "default_str",
            "default",
            "primitive",
            "data",
        ],
        "load_augment" => &[
            "target_tagpath",
            "target_htag",
            "target_namespace",
            "target_hnamespace",
            "target_type",
            "children",
            "actions",
            "notifs",
            "keypath_length",
            "required_set_flags",
            "required_clr_flags",
            "propagate_flags_up",
            "propagate_extra_up",
            "flags",
        ],
        "identity" => &["name", "bases", "flags"],
        "callpoint_info" => &["key", "info"],
        "action" => &["name", "callback", "flags", "extra1", "extra2"],
        _ => &[],
    }
}

fn format_field(record: &str, field: &str, val: &Term, _idx: usize) -> String {
    match (record, field) {
        ("cs", "flags") => format_cs_flags(val),
        ("cs", "exs") => format_inline_record(val),
        ("fxs_header", "yang_header") => format_inline_record(val),
        ("load_type", "flags") => format_load_type_flags(val),
        ("load_augment", "required_set_flags") => format_cs_flags(val),
        ("load_augment", "required_clr_flags") => format_cs_flags(val),
        ("load_augment", "propagate_flags_up") => format_cs_flags(val),
        ("fxs_header", "prefix") => format_charlist(val),
        ("load_type", "data") => format_as_quoted_string(val),
        _ => format_term(val),
    }
}

/// Format a cs flags value (BigInteger/FixInteger) as human-readable flag names.
fn format_cs_flags(val: &Term) -> String {
    let n = term_to_u128(val);
    if n == 0 {
        return format_term(val);
    }
    let names = cs_flag_names(n);
    if names.is_empty() {
        format_term(val)
    } else {
        format!("\"{}\"", names.join(" "))
    }
}

fn cs_flag_names(flags: u128) -> Vec<&'static str> {
    const CS_FLAGS: &[(u128, &str)] = &[
        (1 << 0, "F_CS_IS_LIST"),
        (1 << 1, "F_CS_READ"),
        (1 << 2, "F_CS_WRITE"),
        (1 << 3, "F_CS_VALIDATE"),
        (1 << 4, "F_CS_NACM_DEFAULT_DENY_WRITE"),
        (1 << 5, "F_CS_IS_KEY"),
        (1 << 6, "F_CS_CHILD_SHOW_NO_SET"),
        (1 << 9, "F_CS_NO_DEFAULTS"),
        (1 << 10, "F_CS_IS_CDB"),
        (1 << 11, "F_CS_CHILD_READ_ONLY"),
        (1 << 12, "F_CS_CHILD_VALIDATE"),
        (1 << 13, "F_CS_NACM_DEFAULT_DENY_ALL"),
        (1 << 14, "F_CS_CHILD_READ_WRITE"),
        (1 << 15, "F_CS_CHILD_LIST"),
        (1 << 16, "F_CS_ANCESTOR_HAS_KEYLESS_LIST"),
        (1 << 17, "F_CS_IS_TRANSFORM"),
        (1 << 18, "F_CS_CHILD_TRANSFORM"),
        (1 << 19, "F_CS_WRITE_OPERATIONAL"),
        (1 << 20, "F_CS_IS_CONTAINER"),
        (1 << 21, "F_CS_CHILD_ACTION"),
        (1 << 22, "F_CS_IS_ACTION"),
        (1 << 23, "F_CS_IS_PARAM"),
        (1 << 24, "F_CS_IS_RESULT"),
        (1 << 25, "F_CS_IS_SUBAGENT"),
        (1 << 26, "F_CS_IS_HOOK"),
        (1 << 27, "F_CS_CHILD_HOOK"),
        (1 << 28, "F_CS_NCS_SERVICEPOINT"),
        (1 << 29, "F_CS_IS_NOTIF"),
        (1 << 30, "F_CS_IS_LEAF_LIST"),
        (1 << 31, "F_CS_CHILD_DELETABLE"),
        (1 << 32, "F_CS_SHOW_CONFIG"),
        (1 << 33, "F_CS_CHILD_SHOW_CONFIG"),
        (1 << 34, "F_CS_HAS_CONDITIONAL_DISPLAY"),
        (1 << 35, "F_CS_IS_RELOADABLE"),
        (1 << 36, "F_CS_IS_CASE"),
        (1 << 37, "F_CS_CHILD_OPTIONAL"),
        (1 << 38, "F_CS_CHILD_STATIC_WRITABLE"),
        (1 << 39, "F_CS_USE_OPCACHE"),
        (1 << 40, "F_CS_CLI_SHOW_NO"),
        (1 << 41, "F_CS_HAS_DISPLAY_GROUPS"),
        (1 << 42, "F_CS_IS_CONSTANT"),
        (1 << 43, "F_CS_DYN_VALIDATION"),
        (1 << 44, "F_CS_IS_CASE_DEFAULT"),
        (1 << 45, "F_CS_CLI_NAME"),
        (1 << 46, "F_CS_IS_SET_HOOK"),
        (1 << 47, "F_CS_DESC_IS_HTML"),
        (1 << 48, "F_CS_CHILD_ORDERED_BY"),
        (1 << 49, "F_CS_CHILD_INDEXED_VIEW"),
        (1 << 50, "F_CS_NCS_SERVICE_TEMPLATE"),
        (1 << 51, "F_CS_IS_LEAFREF"),
        (1 << 52, "F_CS_HAS_DISPLAY_DEFAULT_ORDER"),
        (1 << 53, "F_CS_HAS_WHEN"),
        (1 << 54, "F_CS_HAS_WHEN_DEPENDENCY"),
        (1 << 55, "F_CS_HAS_SORT_PRIO"),
        (1 << 56, "F_CS_INHERIT_SET_HOOK"),
        (1 << 57, "F_CS_HAS_PREFIX_LEAF"),
        (1 << 58, "F_CS_JUNOS_AS_TAG"),
        (1 << 59, "F_CS_JUNOS_WITH_PREV_TAG"),
        (1 << 60, "F_CS_IS_ERROR_INFO"),
        (1 << 61, "F_CS_SNMP_SEND_DELETE_VAL"),
        (1 << 62, "F_CS_SNMP_MODIFICATION_DEPENDENCY"),
        (1 << 63, "F_CS_HAS_HIDE_IN_SUBMODE"),
        (1 << 64, "F_CS_CHILD_OPER_ACTION"),
        (1 << 65, "F_CS_CHILD_CONF_ACTION"),
        (1 << 66, "F_CS_CHILD_HAS_RESET"),
        (1 << 67, "F_CS_INDEXED_VIEW"),
        (1 << 68, "F_CS_IS_TOP_KEYLESS_LIST"),
        (1 << 69, "F_CS_IS_SUBAGENT_TOPNODE"),
        (1 << 70, "F_CS_DP_LOWER_CASE"),
        (1 << 71, "F_CS_AUTO_COMPACT"),
        (1 << 72, "F_CS_IS_SYMLINK_TO_LEAFREF"),
        (1 << 73, "F_CS_IS_ANYXML"),
        (1 << 74, "F_CS_SNMP_DELETE_BEFORE_CREATE"),
        (1 << 75, "F_CS_SNMP_RECREATE_WHEN_MODIFIED"),
        (1 << 76, "F_CS_EXPLICIT_DB"),
        (1 << 77, "F_CS_ALWAYS_WRITE"),
        (1 << 78, "F_CS_IMMEDIATE_CHILD_SORT_PRIO"),
        (1 << 79, "F_CS_LSA_SERVICE"),
        (1 << 80, "F_CS_META_DATA"),
        (1 << 81, "F_CS_WRITE_ALL"),
        (1 << 82, "F_CS_NED_DATA"),
        (1 << 83, "F_CS_CHILD_HAS_NED_DATA"),
        (1 << 84, "F_CS_NCS_HAS_PLAN"),
        (1 << 86, "F_CS_NCS_HAS_NANO_PLAN"),
        (1 << 87, "F_CS_NED_IGNORE_CMP_CFG"),
        (1 << 88, "F_CS_HAS_NED_DEFAULT_HANDLING_MODE"),
        (1 << 89, "F_CS_CHILDREN_SAME_CONFIG_DB"),
        (1 << 90, "F_CS_CHILD_DEFAULT"),
        (1 << 91, "F_CS_MOUNT_POINT"),
        (1 << 92, "F_CS_EXS_ACTION"),
        (1 << 93, "F_CS_YANG_STATUS"),
        (1 << 94, "F_CS_CHILD_MANDATORY_OR_DEFAULT"),
        (1 << 95, "F_CS_CHILD_HAS_DIFF_DELETE_AFTER"),
        (1 << 96, "F_CS_IS_WHEN_DEPENDENT"),
        (1 << 97, "F_CS_CHOICE_IS_CONFIG_FALSE"),
        (1 << 98, "F_CS_IMMEDIATE_CHILD_HAS_CLI_NAME"),
        (1 << 99, "F_CS_CHILD_DYN_DEPENDENCY"),
        (1 << 100, "F_CS_HAS_DESCR_POS"),
        (1 << 101, "F_CS_NCS_SERVICE_PRIVATE"),
        (1 << 102, "F_CS_CHILD_UPGRADE_UNCHANGED"),
        (1 << 103, "F_CS_CHILD_NON_STRICT_DYN_DEPENDENCY"),
        (1 << 104, "F_CS_IS_EMPTY_IN_UNION"),
        (1 << 105, "F_CS_CHILD_IS_ENCRYPTED"),
        (1 << 106, "F_CS_NON_STRICT_DYN_VALIDATION"),
        (1 << 107, "F_CS_IS_STRUCTURE"),
        (1 << 108, "F_CS_IS_STRUCTURE_CHILD"),
        (1 << 109, "F_CS_PROMPT"),
        (1 << 110, "F_CS_CHILD_MANDATORY_CHOICE"),
        (1 << 111, "F_CS_CHILD_MIN_ELEMENTS"),
        (1 << 112, "F_CS_CHILD_LEAF_LIST_DEFAULT"),
        (1 << 113, "F_CS_DOC_DESCRIPTION"),
    ];
    let mut names = Vec::new();
    let mut remaining = flags;
    for &(bit, name) in CS_FLAGS {
        if flags & bit != 0 {
            names.push(name);
            remaining &= !bit;
        }
    }
    // Append any unknown bits as hex
    if remaining != 0 {
        names.push("F_CS_UNKNOWN");
    }
    names
}

fn format_load_type_flags(val: &Term) -> String {
    let n = term_to_u128(val);
    if n == 0 {
        return format_term(val);
    }
    const LT_FLAGS: &[(u128, &str)] = &[
        (1 << 0, "F_LOAD_FXS_IS_ENUMERATION"),
        (1 << 1, "F_LOAD_FXS_IS_BITS"),
        (1 << 2, "F_LOAD_FXS_IS_UNION"),
        (1 << 3, "F_LOAD_FXS_MK_DEL_DEPS"),
        (1 << 4, "F_LOAD_FXS_IS_PATTERN"),
        (1 << 5, "F_LOAD_FXS_IS_KEYREF"),
        (1 << 6, "F_LOAD_FXS_IS_IDENTITYREF"),
        (1 << 7, "F_LOAD_FXS_GET_DEFAULT"),
        (1 << 8, "F_LOAD_FXS_IS_EMPTY"),
        (1 << 9, "F_LOAD_FXS_GET_SUPPRESS_ECHO"),
        (1 << 10, "F_LOAD_FXS_GET_TYPE_INFO"),
    ];
    let mut names = Vec::new();
    for &(bit, name) in LT_FLAGS {
        if n & bit != 0 {
            names.push(name);
        }
    }
    if names.is_empty() {
        format_term(val)
    } else {
        format!("\"{}\"", names.join(" "))
    }
}

/// Format a record as an inline string: "record NAME { field = val ... } "
/// Uses compact formatting (no newlines) to keep it single-line.
fn format_inline_record(val: &Term) -> String {
    let tup = match val {
        Term::Tuple(t) => t,
        _ => return format_term(val),
    };
    if tup.elements.is_empty() {
        return format_term(val);
    }
    let tag = match &tup.elements[0] {
        Term::Atom(a) => a.name.clone(),
        _ => return format_term(val),
    };
    let fields = record_fields(&tag);
    if fields.is_empty() {
        return format_term(val);
    }
    let elems = &tup.elements[1..];
    let mut parts = Vec::new();
    for (i, fname) in fields.iter().enumerate() {
        let v = elems.get(i).map_or_else(empty_list, |t| t.clone());
        let fval = if *fname == "flags" && tag == "exs" {
            format_exs_flags(&v)
        } else {
            format_term_compact(&v)
        };
        parts.push(format!("{fname} = {fval}"));
    }
    format!("\"record {tag} {{ {} }} \"", parts.join(", "))
}

/// Compact term formatter — no newlines, used for inline records.
fn format_term_compact(term: &Term) -> String {
    match term {
        Term::Atom(a) => format_atom(&a.name),
        Term::FixInteger(n) => n.value.to_string(),
        Term::BigInteger(b) => b.value.to_string(),
        Term::Float(f) => format!("{}", f.value),
        Term::Binary(b) => format_binary_compact(&b.bytes),
        Term::ByteList(bl) => {
            if bl.bytes.iter().all(|&b| b >= 0x20 && b < 0x7f) {
                format!("\"{}\"", std::str::from_utf8(&bl.bytes).unwrap_or(""))
            } else {
                let items: Vec<String> = bl.bytes.iter().map(|b| b.to_string()).collect();
                format!("[{}]", items.join(","))
            }
        }
        Term::List(list) if list.elements.is_empty() => "[]".to_string(),
        Term::List(list) => {
            if let Some(s) = try_charlist(&list.elements) {
                return format!("\"{}\"", s);
            }
            let items: Vec<String> = list.elements.iter().map(format_term_compact).collect();
            format!("[{}]", items.join(","))
        }
        Term::Tuple(t) => {
            let items: Vec<String> = t.elements.iter().map(format_term_compact).collect();
            format!("{{{}}}", items.join(","))
        }
        Term::Pid(_) => "<pid>".to_string(),
        Term::Port(_) => "<port>".to_string(),
        Term::Reference(_) => "<ref>".to_string(),
        Term::Map(m) => {
            let pairs: Vec<String> = m
                .map
                .iter()
                .map(|(k, v)| format!("{} => {}", format_term_compact(k), format_term_compact(v)))
                .collect();
            format!("#{{{}}}", pairs.join(", "))
        }
        _ => "<unknown>".to_string(),
    }
}

fn format_binary_compact(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "<<>>".to_string();
    }
    if bytes.iter().all(|&b| b >= 0x20 && b < 0x7f) {
        let s = std::str::from_utf8(bytes).unwrap_or("");
        if !s.is_empty() {
            // Escape double-quotes since this is used inside a quoted string.
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            return format!("<<\\\"{escaped}\\\">>");
        }
    }
    let parts: Vec<String> = bytes.iter().map(|b| b.to_string()).collect();
    format!("<<{}>>", parts.join(","))
}

fn format_exs_flags(val: &Term) -> String {
    let n = term_to_u128(val);
    if n == 0 {
        return format_term(val);
    }
    const EXS_FLAGS: &[(u128, &str)] = &[
        (1, "F_EXS_READONLY"),
        (2, "F_EXS_SUPPRESS_ECHO"),
        (4, "F_EXS_IS_ENUMERATION"),
        (8, "F_EXS_IS_LEAF_LIST"),
        (16, "F_EXS_OPTIONAL_NP_CONTAINER"),
        (32, "F_EXS_IS_CRYPTO_TYPE"),
        (64, "F_EXS_IS_MOUNTED"),
    ];
    let mut names = Vec::new();
    for &(bit, name) in EXS_FLAGS {
        if n & bit != 0 {
            names.push(name);
        }
    }
    if names.is_empty() {
        format!("{n}")
    } else {
        format!("\"{}\"", names.join(" "))
    }
}

/// Format a charlist (list of integers) as a quoted string.
fn format_charlist(val: &Term) -> String {
    match val {
        Term::List(list) if list.elements.is_empty() => "\"\"".to_string(),
        Term::ByteList(bl) => {
            if bl.bytes.iter().all(|&b| b >= 0x20 && b < 0x7f) {
                format!("\"{}\"", std::str::from_utf8(&bl.bytes).unwrap_or(""))
            } else {
                format_term(val)
            }
        }
        Term::List(list) => {
            // Try to interpret as a charlist (list of printable ASCII integers)
            let chars: Option<String> = list
                .elements
                .iter()
                .map(|e| match e {
                    Term::FixInteger(n) if n.value >= 0 && n.value <= 127 => {
                        char::from_u32(n.value as u32)
                    }
                    _ => None,
                })
                .collect();
            match chars {
                Some(s) => format!("\"{}\"", s),
                None => format_term(val),
            }
        }
        _ => format_term(val),
    }
}

/// Format a term as a quoted Erlang string representation (for data fields
/// like load_type.data which confdc prints as a string).
fn format_as_quoted_string(val: &Term) -> String {
    format!("\"{}\"", format_term(val))
}

fn term_to_u128(val: &Term) -> u128 {
    match val {
        Term::FixInteger(n) if n.value >= 0 => n.value as u128,
        Term::BigInteger(b) => b.value.to_string().parse::<u128>().unwrap_or(0),
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Generic Erlang term formatter (mimics Erlang's ~p format)
// ---------------------------------------------------------------------------

fn format_term(term: &Term) -> String {
    match term {
        Term::Atom(a) => format_atom(&a.name),
        Term::FixInteger(n) => n.value.to_string(),
        Term::BigInteger(b) => b.value.to_string(),
        Term::Float(f) => format!("{}", f.value),
        Term::Binary(b) => format_binary(&b.bytes),
        Term::ByteList(bl) => {
            // ByteList is a charlist in Erlang (list of bytes as a string)
            if bl.bytes.iter().all(|&b| b >= 0x20 && b < 0x7f) {
                format!("\"{}\"", std::str::from_utf8(&bl.bytes).unwrap_or(""))
            } else {
                let items: Vec<String> = bl.bytes.iter().map(|b| b.to_string()).collect();
                format!("[{}]", items.join(","))
            }
        }
        Term::List(list) if list.elements.is_empty() => "[]".to_string(),
        Term::List(list) => {
            // Try charlist first
            if let Some(s) = try_charlist(&list.elements) {
                return format!("\"{}\"", s);
            }
            let items: Vec<String> = list.elements.iter().map(format_term).collect();
            format!("[{}]", items.join(",\n               "))
        }
        Term::Tuple(t) => {
            let items: Vec<String> = t.elements.iter().map(format_term).collect();
            format!("{{{}}}", items.join(",\n                   "))
        }
        Term::Pid(_) => "<pid>".to_string(),
        Term::Port(_) => "<port>".to_string(),
        Term::Reference(_) => "<ref>".to_string(),
        Term::Map(m) => {
            let pairs: Vec<String> = m
                .map
                .iter()
                .map(|(k, v)| format!("{} => {}", format_term(k), format_term(v)))
                .collect();
            format!("#{{{}}}", pairs.join(", "))
        }
        _ => "<unknown>".to_string(),
    }
}

fn format_atom(name: &str) -> String {
    // Quote if needed (contains special chars or starts with uppercase)
    let needs_quote = name.is_empty()
        || name.starts_with(|c: char| c.is_uppercase())
        || name.contains(|c: char| !c.is_alphanumeric() && c != '_' && c != '@')
        || ERLANG_KEYWORDS.contains(&name);
    if needs_quote {
        format!("'{}'", name.replace('\\', "\\\\").replace('\'', "\\'"))
    } else {
        name.to_string()
    }
}

const ERLANG_KEYWORDS: &[&str] = &[
    "after", "and", "andalso", "band", "begin", "bnot", "bor", "bsl", "bsr", "bxor", "case",
    "catch", "cond", "div", "end", "fun", "if", "let", "not", "of", "or", "orelse", "query",
    "receive", "rem", "try", "when", "xor",
];

fn format_binary(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "<<>>".to_string();
    }
    // Check if it looks like a printable string (for revision binaries etc.)
    if bytes.iter().all(|&b| b >= 0x20 && b < 0x7f) {
        let s = std::str::from_utf8(bytes).unwrap_or("");
        if !s.is_empty() {
            return format!("<<\"{}\">>", s);
        }
    }
    let parts: Vec<String> = bytes.iter().map(|b| b.to_string()).collect();
    format!("<<{}>>", parts.join(","))
}

fn try_charlist(elements: &[Term]) -> Option<String> {
    if elements.is_empty() {
        return None;
    }
    let mut chars = String::new();
    for e in elements {
        match e {
            Term::FixInteger(n) if n.value >= 32 && n.value <= 126 => {
                chars.push(n.value as u8 as char);
            }
            _ => return None,
        }
    }
    Some(chars)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: fxs-print FILE [FILE...]");
        std::process::exit(1);
    }

    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    for path in &args {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("fxs-print: {path}: {e}");
                std::process::exit(1);
            }
        };

        // Print filename header when processing multiple files.
        if args.len() > 1 {
            writeln!(
                out,
                "=== {} ===",
                Path::new(path).file_name().unwrap().to_string_lossy()
            )
            .unwrap();
        }

        match parse_fxs(&data) {
            Ok(fxs) => print_fxs(&fxs, &mut out),
            Err(e) => {
                eprintln!("fxs-print: {path}: parse error: {e}");
                std::process::exit(1);
            }
        }
    }
}
