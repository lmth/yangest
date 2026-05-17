//! Schema tree walker: produces `#cs{}` and `#hash{}` Erlang records from a
//! yangest `CompiledModule`.
//!
//! Mirrors `yanger_fxs:fill_tab/5` and `yanger_fxs:add_cs0/9`.
//!
//! **cs record ordering**: The FXS file stores cs records such that after the
//! `fxs_write_list` reversal, `confdc --print-fxs` prints deepest nodes first
//! and root last.  yanger_fxs builds CsL with root first (pre-order pre-pend):
//!   CsL = [root_cs, top_cs, name_cs]
//!   reversed in file = [name_cs, top_cs, root_cs]
//!   confdc prints = name, top, root
//!
//! We achieve the same by pushing a placeholder at the parent's position, then
//! walking children (which push their own cs records), then replacing the
//! placeholder with the final cs built from child aggregate info.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use eetf::Term;
use num_bigint::{BigInt, BigUint};
use yangest_core::ast::Keyword;
use yangest_core::compiler::{
    CompiledModule, ExpansionCtx, ExtensionInstance, ModuleRegistry, NodeOverlayMap, OrderedBy,
    PathStep, SchemaNode, SchemaNodeKind, Status, expand_children, find_child_in_raw,
};

use crate::xpath_compiler::{build_must_vmfas, build_when_extra};

#[cfg(feature = "yanger-compat-hash-order")]
use crate::genie::{dict_fold_order_terms, term::Term as GenieTerm};
use crate::hash::phash2_atom;
use crate::terms::{
    atom, bigint, bigint_bigint, binary_str, charlist, improper_list_pair, int, list, make_cs,
    make_exs, make_hash_record, make_load_augment, nil, tuple, uint, undefined,
};
use crate::thash::{
    MiscEntry, TypeGen, bits_primitive_atom, bits_type_size, encode_fraction_digits_facet_bytes,
    encode_length_facet_bytes, encode_pattern_facet_bytes, encode_range_facet_bytes,
    fraction_digits_facet_eetf, length_facet_eetf, parse_length_ranges, parse_range_bounds,
    pattern_facet_eetf, range_facet_eetf, yang_int_to_xsd_info,
};
use crate::types::{
    F_EXS_IS_LEAF_LIST, F_EXS_OPTIONAL_NP_CONTAINER, F_EXS_READONLY, is_empty_base, is_union_base,
    type_info_from_stmt_arg, type_info_with_registry,
};

/// From cs.hrl: set on cross-module typedef leaves so the loader fetches the type.
const F_LOAD_FXS_GET_SUPPRESS_ECHO: u32 = 1 << 9;
const F_LOAD_FXS_GET_TYPE_INFO: u32 = 1 << 10;
/// Set when a cross-module typedef leaf has a default value (non-enum types).
const F_LOAD_FXS_GET_DEFAULT: u32 = 1 << 7;
/// Set when a cross-module typedef leaf has a default value for enum types.
/// Enum defaults are parsed at load time (internal_enum_hash behavior).
const F_LOAD_FXS_PARSE_DEFAULT: u32 = 1 << 8;
/// Set when a typedef is a pure "identity derivation": it references another typedef
/// without adding any restrictions (same type spec as the base typedef).
const F_LOAD_FXS_IS_IDENTITY_DERIVATION: u32 = 1 << 16;
/// Set when the typedef's base type is a union type.
const F_LOAD_FXS_IS_UNION: u32 = 1 << 2;
/// Set when the typedef's base type is the empty type.
const F_LOAD_FXS_IS_EMPTY: u32 = 1 << 5;

// ---------------------------------------------------------------------------
// cs.flags constants (from cs.hrl)
// ---------------------------------------------------------------------------
const F_CS_IS_LIST: u128 = 1 << 0;
const F_CS_READ: u128 = 1 << 1;
const F_CS_WRITE: u128 = 1 << 2;
const F_CS_IS_KEY: u128 = 1 << 5;
const F_CS_CHILD_SHOW_NO_SET: u128 = 1 << 6;
const F_CS_NO_DEFAULTS: u128 = 1 << 9;
const F_CS_IS_CDB: u128 = 1 << 10;
const F_CS_CHILD_READ_WRITE: u128 = 1 << 14;
const F_CS_CHILD_LIST: u128 = 1 << 15;
const F_CS_IS_CONTAINER: u128 = 1 << 20;
const F_CS_IS_ACTION: u128 = 1 << 22;
const F_CS_IS_NOTIF: u128 = 1 << 29;
const F_CS_IS_LEAF_LIST: u128 = 1 << 30;
const F_CS_CHILD_DELETABLE: u128 = 1 << 31;
const F_CS_CHILD_SHOW_CONFIG: u128 = 1 << 33;
const F_CS_IS_CASE: u128 = 1 << 36;
const F_CS_IS_CASE_DEFAULT: u128 = 1 << 44;
const F_CS_CHILD_OPTIONAL: u128 = 1 << 37;
const F_CS_CLI_NAME: u128 = 1 << 45;
const F_CS_CHILD_OPER_ACTION: u128 = 1 << 64;
const F_CS_CHILD_CONF_ACTION: u128 = 1 << 65;
const F_CS_CHILD_HAS_RESET: u128 = 1 << 66;
const F_CS_CHILDREN_SAME_CONFIG_DB: u128 = 1 << 89;
const F_CS_CHILD_DEFAULT: u128 = 1 << 90;
const F_CS_YANG_STATUS: u128 = 1 << 93;
const F_CS_ANCESTOR_HAS_KEYLESS_LIST: u128 = 1 << 16;
const F_CS_IS_TOP_KEYLESS_LIST: u128 = 1 << 68;
// Oper-specific flags
const F_CS_WRITE_OPERATIONAL: u128 = 1 << 19;
const F_CS_CHILD_READ_ONLY: u128 = 1 << 11;
const F_CS_WRITE_ALL: u128 = 1 << 81;
const F_CS_CHILD_MANDATORY_OR_DEFAULT: u128 = 1 << 94;
const F_CS_CHILD_HAS_DIFF_DELETE_AFTER: u128 = 1 << 95;
const F_CS_CHILD_MANDATORY_CHOICE: u128 = 1 << 110;
const F_CS_CHILD_ORDERED_BY: u128 = 1 << 48;
const F_CS_IS_PARAM: u128 = 1 << 23;
const F_CS_IS_RESULT: u128 = 1 << 24;
// when-condition flags
const F_CS_HAS_WHEN: u128 = 1 << 53;
const F_CS_IS_WHEN_DEPENDENT: u128 = 1 << 96;
const F_CS_META_DATA: u128 = 1 << 80;
// doc description flag: set when node has description or tailf:info text
const F_CS_DOC_DESCRIPTION: u128 = 1 << 113;
// F_CS_HAS_PREFIX_LEAF: set on a list/container when a child leaf has tailf:cli-prefix-key
const F_CS_HAS_PREFIX_LEAF: u128 = 1 << 57;
// F_CS_IMMEDIATE_CHILD_HAS_CLI_NAME: set on a parent when an immediate child has tailf:alt-name (F_CS_CLI_NAME)
const F_CS_IMMEDIATE_CHILD_HAS_CLI_NAME: u128 = 1 << 98;

// F_CLI_* bits used to compute my_child_flags (bits 0-127 only; bits 128+ use cli_words directly)
const F_CLI_SHOW_NO: u128 = 1 << 0;
const F_CLI_SHOW_CONFIG: u128 = 1 << 1;
const F_CLI_SHOW_WITH_DEFAULT: u128 = 1 << 79;
const F_CLI_CONFIGURE_MODE: u128 = 1 << 105;

// ---------------------------------------------------------------------------
// Mandatory-child check (for F_EXS_OPTIONAL_NP_CONTAINER)
// ---------------------------------------------------------------------------

/// Returns true if a choice node is "optional" per yanger_fxs `is_optional_choice`.
///
/// A choice is mandatory if it has `mandatory true` AND no explicit `config false`
/// on its own statement (i.e. `node.config != Some(false)`). This mirrors the
/// Erlang logic: `get_substmt_arg(stmt,'mandatory',false) andalso
///               get_substmt_arg(stmt,'config',true)`.
fn is_optional_choice(node: &SchemaNode) -> bool {
    if let SchemaNodeKind::Choice { mandatory, .. } = &node.kind {
        // is_mandatory_choice = mandatory=true AND no explicit config=false on THIS stmt
        let is_mandatory = *mandatory && node.config != Some(false);
        !is_mandatory
    } else {
        true
    }
}

/// Determines whether `node` contributes `ismandatory = true` to its parent container,
/// mirroring yanger_fxs's `ChildIsMandatory` accumulation logic.
///
/// `optional_choice_ctx` corresponds to `S0#state.optional_choice` in yanger_fxs: it is
/// `true` when this node is being processed as a descendant of an *optional* choice's case.
///
/// Key rules (from yanger_fxs `add_cs0` and `add_cs_sn`):
/// - config=false (oper) nodes always return false.
/// - Choice: if `!optional_choice_ctx` → always true; if `optional_choice_ctx` → recurse.
/// - Case: transparent, pass context through.
/// - NP container: propagates accumulated child result.
/// - Presence container: false (min_occurs=0).
/// - Mandatory leaf / leaf with min_occurs=1: true.
/// - List/leaf-list with min_elements>=1: true.
fn child_is_mandatory(
    node: &SchemaNode,
    optional_choice_ctx: bool,
    mode: SubtreeMode,
    ctx: &ExpansionCtx<'_>,
) -> bool {
    // Choices in yanger_fxs use add_cs0 which does NOT check config=false.
    // They always return ismandatory=true when not optional_choice_ctx, regardless of oper status.
    if let SchemaNodeKind::Choice { .. } = &node.kind {
        if !optional_choice_ctx {
            // S0.optional_choice=false → choice always returns ismandatory=true (yanger_fxs line 1004-1005)
            return true;
        }
        // S0.optional_choice=true → accumulate from case children
        // The choice sets S1.optional_choice = is_optional_choice(choice)
        let new_ctx = is_optional_choice(node);
        let cases = node.children(ctx);
        return cases
            .iter()
            .any(|case| child_is_mandatory(case, new_ctx, mode, ctx));
    }

    // For all non-choice nodes: oper nodes never contribute to mandatory
    // (per yanger_fxs add_cs_sn line 1554: "if Sn#sn.config == false -> false")
    let effective_mode = match (mode, node.config) {
        (SubtreeMode::Config, Some(false)) => SubtreeMode::Oper,
        (SubtreeMode::Oper, Some(true)) => SubtreeMode::Config,
        _ => mode,
    };
    if effective_mode.is_oper() {
        return false;
    }

    match &node.kind {
        SchemaNodeKind::Choice { .. } => unreachable!(), // handled above
        SchemaNodeKind::Case { .. } => {
            let children = node.children(ctx);
            children
                .iter()
                .any(|ch| child_is_mandatory(ch, optional_choice_ctx, effective_mode, ctx))
        }
        SchemaNodeKind::Container { presence, .. } => {
            if presence.is_some() {
                false
            } else {
                let children = node.children(ctx);
                children
                    .iter()
                    .any(|ch| child_is_mandatory(ch, optional_choice_ctx, effective_mode, ctx))
            }
        }
        SchemaNodeKind::Leaf { mandatory, .. } => *mandatory,
        SchemaNodeKind::LeafList { min_elements, .. } => *min_elements >= 1,
        SchemaNodeKind::List { min_elements, .. } => *min_elements >= 1,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Subtree mode for action input/output
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum SubtreeMode {
    /// Normal config subtree
    Config,
    /// Operational data subtree  
    Oper,
    /// Inside action/RPC input: nodes get F_CS_IS_PARAM
    ActionInput,
    /// Inside action/RPC output: nodes get F_CS_IS_RESULT
    ActionOutput,
    /// Inside a notification
    Notification,
}

impl SubtreeMode {
    fn rw_flags(self) -> u128 {
        match self {
            SubtreeMode::Config => F_CS_READ | F_CS_WRITE,
            SubtreeMode::Oper => F_CS_READ,
            SubtreeMode::ActionInput => F_CS_READ | F_CS_IS_PARAM,
            SubtreeMode::ActionOutput => F_CS_IS_RESULT,
            SubtreeMode::Notification => 0,
        }
    }
    fn is_config(self) -> bool {
        matches!(self, SubtreeMode::Config)
    }
    fn is_oper(self) -> bool {
        matches!(self, SubtreeMode::Oper)
    }
    fn is_notification(self) -> bool {
        matches!(self, SubtreeMode::Notification)
    }
    fn is_data_mode(self) -> bool {
        matches!(self, SubtreeMode::Config | SubtreeMode::Oper)
    }
    /// True when list-key semantics apply: data tree or action/RPC input (NOT output or notification).
    /// Mirrors yanger_fxs: `is_data_tree_or_input = SubtreeMode /= output AND SubtreeMode /= notification`
    fn is_data_tree_or_input(self) -> bool {
        !matches!(self, SubtreeMode::ActionOutput | SubtreeMode::Notification)
    }
}

// ---------------------------------------------------------------------------
// Walk result
// ---------------------------------------------------------------------------

pub struct WalkResult {
    /// All `#cs{}` terms in pre-order (root first, deepest last).
    /// The `fxs_write_list` reversal will invert this so the FXS file stores
    /// them deepest-first as confdc expects.
    pub cs_records: Vec<Term>,
    /// `#hash{}` terms for all named nodes, in `dict:fold/3` order.
    pub hash_records: Vec<Term>,
    /// `{identity, {identity, Name}, Bases, 0}` terms for all identities defined in this module.
    pub identity_records: Vec<Term>,
    /// `#exs_type{}` terms for typedefs defined in this module.
    pub exs_type_records: Vec<Term>,
    /// `#load_type{}` terms for typedefs defined in this module.
    pub load_type_records: Vec<Term>,
    /// `#exs_type{}` terms for t<hash> anonymous types generated during schema walk (DFS order).
    pub generated_exs_type_records: Vec<Term>,
    /// `#load_type{}` terms for inline t<hash> types generated during schema walk (DFS order).
    pub generated_load_type_records: Vec<Term>,
    /// `#exs_type{}` terms for t<hash> anonymous types generated during typedef processing
    /// (e.g., inline enum members of union typedefs). These appear AFTER named typedef records.
    pub typedef_inline_exs_type_records: Vec<Term>,
    /// Unified Misc2 records: action records and node doc records, in reversed-DFS order
    /// (as pushed during walk_node which is reversed DFS).  Written to Misc2 in reversed
    /// order so that after fxs_write_list reversal they appear in forward DFS order in the file.
    pub misc_records: Vec<Term>,
    /// Names of `tailf:actionpoint` callpoints found in RPC/action nodes.
    /// Each entry is the actionpoint name atom used in callpoint_info.
    pub actionpoint_names: Vec<String>,
    /// `{load_augment, ...}` records for external augments (AugL section).
    pub load_augment_records: Vec<Term>,
    /// Doc records for enum/bit values with tailf:info (from type Misc).
    /// These are written to the FXS Misc2 section alongside the module doc.
    pub type_doc_records: Vec<Term>,
    /// True when any CDB-config node exists.
    pub has_cdb: bool,
    /// True when any CDB-operational node exists.
    pub has_cdb_oper: bool,
    pub max_keypath_length: u32,
    pub max_key_tuple_size: u32,
}

pub fn walk_module(module: &CompiledModule, ctx: &ExpansionCtx<'_>) -> WalkResult {
    let ns = &module.namespace;
    let ns_hash = phash2_atom(ns) as u128;

    let children = module.children(ctx);

    // Separate notification children from config/data children for root cs fields.
    let (notif_children, config_children): (Vec<_>, Vec<_>) = children
        .iter()
        .partition(|ch| matches!(ch.kind, SchemaNodeKind::Notification { .. }));

    // Root exs.children: non-RPC children first (in YANG source order), then RPC names appended
    // last — matching yanger's `ExsChildren0 ++ Rpcs` in fill_tab/6.
    let (rpc_children, data_children): (Vec<_>, Vec<_>) = config_children
        .iter()
        .partition(|ch| matches!(ch.kind, SchemaNodeKind::Rpc { .. }));
    let mut exs_children: Vec<Term> = data_children
        .iter()
        .map(|&ch| mk_exs_child_term(ch, true, ctx))
        .collect();
    for rpc in &rpc_children {
        exs_children.push(atom(&rpc.name));
    }

    let root_exs = make_exs(
        nil(),
        undefined(),
        undefined(),
        undefined(),
        nil(),
        int(1),
        int(1),
        list(exs_children),
        int(0),
        nil(),
    );

    let mut state = WalkState {
        cs_records: Vec::new(),
        hash_records: Vec::new(),
        hash_keys: Vec::new(),
        hash_seen: HashSet::new(),
        misc_records: Vec::new(),
        actionpoint_names: Vec::new(),
        has_cdb: false,
        has_cdb_oper: false,
        max_keypath_length: 0,
        max_key_tuple_size: 0,
        file_module_name: module.key.name.clone(),
        type_gen: TypeGen::new(),
        inherited_hidden: atom("none"),
        module_ns_cache: ctx.registry.modules.values()
            .map(|m| (m.key.name.clone(), m.namespace.clone()))
            .collect(),
        not_supported_paths: collect_deviate_not_supported_paths(ctx.registry),
    };

    // Pre-populate TypeGen with generated types in FORWARD DFS order.
    // This matches yanger_fxs's accumulation pattern and ensures the ExsTypes
    // section has types in the correct forward-DFS order.
    for child in children.iter() {
        collect_types_forward(child, ns, &module.key.name, ctx, &mut state.type_gen, &state.module_ns_cache);
    }

    // Pre-register inline enumeration types from grouping definitions that are NOT
    // used locally (not expanded into the schema tree). This mirrors yanger's
    // add_enumeration_types() / add_unused_groupings() which processes groupings whose
    // enums never appear in add_cs0 (no local `uses`), registering them with
    // LoadTypeFlags=0 → undefined LoadType → no load_type record.
    //
    // Must run AFTER collect_types_forward AND after walk_augments (below) so that
    // enums from USED groupings — whether used directly or via augments — are already
    // registered with IS_ENUM hash + load_type and the pre-pass simply skips them.
    // (Moved to after walk_augments below.)

    // Phase 1: Forward-DFS hash pre-pass.
    // Populates state.hash_records and state.hash_seen in declaration order.
    // This must run BEFORE the reverse-order CS walk so that the first-occurrence
    // rule mirrors yanger_fxs's dict behavior (first sibling's leaves win, not last).
    state.hash_records.push(make_hash_record(
        tuple(vec![atom("hash_uri"), atom(ns)]),
        atom("uri"),
        bigint(ns_hash),
        int(0),
        atom("false"),
    ));
    state.hash_seen.insert(ns.as_bytes().to_vec());
    state.hash_keys.push(ns.as_bytes().to_vec());

    // Build identity records (for Identities section) and their hash records.
    let (identity_records, identity_hash_records, identity_hash_keys) =
        build_identity_records(module, ctx);

    // Add identity hash records first (before schema node hashes).
    for (rec, key) in identity_hash_records.iter().zip(identity_hash_keys.iter()) {
        if state.hash_seen.insert(key.clone()) {
            state.hash_records.push(rec.clone());
            state.hash_keys.push(key.clone());
        }
    }

    // Then collect schema node hashes in forward DFS order.
    for child in children.iter() {
        collect_hashes_forward(child, ns, &[], None, None, ctx, &mut state);
    }

    // Phase 2: Walk all module-level children in REVERSED order (pre-order reversed-children traversal).
    // This matches yanger_fxs's list-building order so that after fxs_write_list reversal
    // the FXS file stores cs records with root first and deepest nodes last.
    // Hash records are NOT generated here (already done in Phase 1).
    for child in children.iter().rev() {
        walk_node(
            child,
            None, // top-level: no parent
            ns,
            ns_hash,
            &[],
            &[],
            SubtreeMode::Config,
            0,
            false,
            false,
            -1,
            None,
            false,
            None,
            module,
            ctx,
            &mut state,
        );
    }

    // Phase 2b: Walk external augments (nodes augmented into other modules).
    // Hash pre-registration for augmented nodes happens here too (forward DFS order,
    // after the main schema node hashes to match yanger's insertion order).
    // Augmented CS records are added to the main cs_records pool (CsCdbL/CsL sections).
    // load_augment metadata records go in the AugL section.
    //
    // Record where augmented CS records begin so they can be reordered after the walk.
    // yanger_fxs prepends own nodes first, then augments on top, resulting in augmented
    // nodes appearing BEFORE own nodes in the fxs-print display order.
    let aug_records_start = state.cs_records.len();
    let load_augment_records = walk_augments(module, ctx, &mut state);

    // Pre-register inline enumeration types from grouping definitions that are NOT
    // used (not expanded into the schema tree — either directly or through augments).
    // Must run AFTER collect_types_forward AND walk_augments so all used enums are
    // already registered with IS_ENUM hash + load_type and the pre-pass skips them.
    for (_, grouping) in module.groupings.iter() {
        if grouping.definer_module_name == module.key.name {
            pre_register_grouping_enum_types(&grouping.stmt, ns, &mut state.type_gen);
        }
    }

    let mut root_flags = F_CS_READ | F_CS_WRITE;

    // notifs: list of module-level notification names, or [] when there are none.
    let notifs_term = list(notif_children.iter().map(|ch| atom(&ch.name)).collect());

    // Extract tailf:hidden and tailf:meta-data from module-level extensions.
    let (root_hidden, root_meta_items, tailf_flags) = tailf_hidden_and_meta(&module.extensions);
    root_flags |= tailf_flags;
    let root_extra = if root_meta_items.is_empty() {
        nil()
    } else {
        list(root_meta_items)
    };

    let root_cs = make_cs(
        nil(),
        nil(),
        atom(ns),
        bigint(ns_hash),
        root_exs,
        nil(),
        bigint(root_flags),
        undefined(),
        nil(),
        nil(),
        nil(),
        int(0),
        nil(),
        root_hidden,
        notifs_term,
        undefined(),
        root_extra,
        undefined(),
        nil(),
        int(0),
        undefined(),
        nil(),
        nil(),
    );
    state.cs_records.push(root_cs);

    // Reorder cs_records to match yanger_fxs's display order:
    //   [augmented_nodes..., own_nodes..., root_cs]
    // yanger builds by prepending own nodes first, then augments on top, so augmented
    // nodes end up BEFORE own nodes in the write_list input (fxs-print display) order.
    // Our push-based code has: [own_nodes..., aug_nodes..., root_cs]; swap aug before own.
    if aug_records_start < state.cs_records.len().saturating_sub(1) {
        let root = state.cs_records.pop().unwrap();
        let aug_nodes: Vec<Term> = state.cs_records.drain(aug_records_start..).collect();
        let own_nodes: Vec<Term> = state.cs_records.drain(..).collect();
        state.cs_records.extend(aug_nodes);
        state.cs_records.extend(own_nodes);
        state.cs_records.push(root);
    }

    // Build ordered_hash: with the yanger-compat-hash-order feature, reorder entries
    // to match yanger's exact Erlang dict fold order (cosmetic, for byte-identical output).
    // Without the feature, emit in the forward-DFS insertion order built by collect_hashes_forward.
    #[cfg(feature = "yanger-compat-hash-order")]
    let ordered_hash = {
        // yanger insertion order: NS URI first, then identities, then schema nodes.
        let mut yanger_insert_keys: Vec<GenieTerm> = Vec::new();
        let mut yanger_seen: HashSet<Vec<u8>> = HashSet::new();
        yanger_insert_keys.push(GenieTerm::Atom(ns.as_bytes().to_vec()));
        yanger_seen.insert(ns.as_bytes().to_vec());
        for k in &identity_hash_keys {
            if yanger_seen.insert(k.clone()) {
                yanger_insert_keys.push(GenieTerm::Atom(k.clone()));
            }
        }
        for child in children.iter() {
            collect_hash_keys_forward(child, ctx, &mut yanger_insert_keys, &mut yanger_seen);
        }
        // Also include augmented node hash keys (added by walk_augments after the main tree walk).
        // yanger inserts these via mk_augment_cs after the main cs walk, so they appear later in
        // the insertion sequence. We add any state.hash_keys not yet in yanger_seen.
        for k in &state.hash_keys {
            if yanger_seen.insert(k.clone()) {
                yanger_insert_keys.push(GenieTerm::Atom(k.clone()));
            }
        }

        let name_to_hash_idx: HashMap<Vec<u8>, usize> = state
            .hash_keys
            .iter()
            .enumerate()
            .map(|(i, k)| (k.clone(), i))
            .collect();

        let yanger_fold_indices = dict_fold_order_terms(&yanger_insert_keys);
        let mut out: Vec<Term> = Vec::with_capacity(state.hash_records.len());
        for i in yanger_fold_indices {
            let key_bytes = match &yanger_insert_keys[i] {
                GenieTerm::Atom(b) => b,
                _ => panic!(),
            };
            let hash_idx = name_to_hash_idx[key_bytes.as_slice()];
            out.push(state.hash_records[hash_idx].clone());
        }
        out
    };
    #[cfg(not(feature = "yanger-compat-hash-order"))]
    let ordered_hash = state.hash_records.clone();

    // Generate exs_type and load_type records for typedefs defined in this module.
    // Also populates state.type_gen with any t<hash> types from inline union member types.
    // Record split point so we can separate schema-walk types from typedef-inline types.
    let schema_walk_type_count = state.type_gen.len();
    let (exs_type_records, load_type_records) =
        generate_typedef_records(module, ctx.registry, &mut state.type_gen, &state.module_ns_cache);

    WalkResult {
        cs_records: state.cs_records,
        hash_records: ordered_hash,
        identity_records,
        exs_type_records,
        load_type_records,
        generated_exs_type_records: state.type_gen.exs_type_terms_range(0, schema_walk_type_count),
        generated_load_type_records: state.type_gen.load_type_terms_range(0, schema_walk_type_count),
        typedef_inline_exs_type_records: vec![], // now interleaved into exs_type_records by generate_typedef_records
        misc_records: state.misc_records,
        actionpoint_names: state.actionpoint_names,
        load_augment_records,
        type_doc_records: state.type_gen.doc_misc_records,
        has_cdb: state.has_cdb,
        has_cdb_oper: state.has_cdb_oper,
        max_keypath_length: state.max_keypath_length,
        max_key_tuple_size: state.max_key_tuple_size,
    }
}

/// Build identity records and their hash records for all identities defined in this module.
///
/// Returns:
/// - identity_records: `{identity, {identity, Name}, [{BaseModule, BaseName}], 0}` for Identities section
/// - identity_hash_records: `{hash, {hash, Name}, identity, Hash, 0, false}` for HashDict section
/// - identity_hash_keys: atom name bytes for dict ordering (same as identity names, alphabetical order)
///
/// Identity records are sorted in reverse alphabetical order, matching yanger_fxs which
/// accumulates them via prepending during an alphabetical map_foldl, resulting in reverse-alpha
/// order. `write_list` then reverses them on disk to give alphabetical order.
///
/// Identity hash keys are in alphabetical order, matching yanger_fxs `add_hash` insertion order.
fn build_identity_records(
    module: &CompiledModule,
    ctx: &ExpansionCtx<'_>,
) -> (Vec<Term>, Vec<Term>, Vec<Vec<u8>>) {
    let own_prefix = &module.prefix;

    // Sort identities alphabetically by name (matching yang:map_foldl order in yanger_fxs).
    let mut sorted_identities: Vec<&yangest_core::compiler::Identity> =
        module.identities.values().collect();
    sorted_identities.sort_by(|a, b| a.name.cmp(&b.name));

    // Build hash records in alphabetical order (matching yanger_fxs add_hash insertion order).
    let mut hash_records = Vec::new();
    let mut hash_keys: Vec<Vec<u8>> = Vec::new();
    for identity in &sorted_identities {
        let id_hash = phash2_atom(&identity.name) as u128;
        let hash_record = make_hash_record(
            tuple(vec![atom("hash"), atom(&identity.name)]),
            atom("identity"),
            bigint(id_hash),
            int(0),
            atom("false"),
        );
        hash_records.push(hash_record);
        hash_keys.push(identity.name.as_bytes().to_vec());
    }

    // Build identity_records in reverse alphabetical order. Since write_list reverses its input,
    // the on-disk representation will be in alphabetical order.
    let mut identity_records = Vec::new();
    for identity in sorted_identities.iter().rev() {
        // Build the base list: [{ModuleAtom | undefined, BaseNameAtom}]
        let bases: Vec<Term> = identity
            .bases
            .iter()
            .map(|(opt_prefix, base_name)| {
                let base_mod_term = match opt_prefix {
                    None => undefined(),
                    Some(prefix) if prefix == own_prefix => undefined(),
                    Some(prefix) => {
                        // Resolve prefix to module name, then to namespace URI.
                        // yanger_fxs uses yanger_fxs_ns:get_table_id which returns the
                        // namespace URI of the imported module, not its name.
                        match module.prefix_map.get(prefix.as_str()) {
                            Some(mod_name) => match ctx.registry.resolve_import(mod_name, None) {
                                Some(imported_mod) => atom(&imported_mod.namespace),
                                None => atom(mod_name),
                            },
                            None => undefined(),
                        }
                    }
                };
                tuple(vec![base_mod_term, atom(base_name)])
            })
            .collect();

        let identity_term = tuple(vec![
            atom("identity"),
            tuple(vec![atom("identity"), atom(&identity.name)]),
            list(bases),
            int(0),
        ]);
        identity_records.push(identity_term);
    }

    (identity_records, hash_records, hash_keys)
}

/// Collect schema node names in forward DFS pre-order, matching the order
/// yanger_fxs calls `add_hash` (parent before children, children in declaration order).
/// Only used by the yanger-compat-hash-order feature.
/// `seen` tracks first-occurrence deduplication, mirroring yanger's dict behavior.
#[cfg(feature = "yanger-compat-hash-order")]
fn collect_hash_keys_forward(
    node: &SchemaNode,
    ctx: &ExpansionCtx<'_>,
    keys: &mut Vec<GenieTerm>,
    seen: &mut HashSet<Vec<u8>>,
) {
    let name_bytes = node.name.as_bytes().to_vec();
    if seen.insert(name_bytes.clone()) {
        keys.push(GenieTerm::Atom(name_bytes));
    }
    match &node.kind {
        SchemaNodeKind::Rpc { .. } | SchemaNodeKind::Action { .. } => {
            // In yanger, an RPC's sn.children order determines hash registration order.
            // When the RPC has EXPLICIT output content: input_sn comes first in sn.children
            //   (YANG declaration order: input before output), so:
            //   input children → $output → output children.
            // When the RPC has NO explicit output (empty/synthetic): yanger places the
            //   synthetic output_sn BEFORE input_sn, so:
            //   $output → input children.
            let input = node.input_children(ctx);
            let output = node.output_children(ctx);
            let output_key = b"$output".to_vec();
            if output.is_empty() {
                // No explicit output: $output registered first (synthetic output_sn before input_sn)
                if seen.insert(output_key.clone()) {
                    keys.push(GenieTerm::Atom(output_key));
                }
                for child in &input {
                    collect_hash_keys_forward(child, ctx, keys, seen);
                }
            } else {
                // Explicit output: input children first, then $output, then output children
                for child in &input {
                    collect_hash_keys_forward(child, ctx, keys, seen);
                }
                if seen.insert(output_key.clone()) {
                    keys.push(GenieTerm::Atom(output_key));
                }
                for child in &output {
                    collect_hash_keys_forward(child, ctx, keys, seen);
                }
            }
        }
        _ => {
            let children = node.children(ctx);
            for child in &children {
                collect_hash_keys_forward(child, ctx, keys, seen);
            }
        }
    }
}

/// Forward-DFS hash pre-pass: register all hash records in declaration order.
///
/// Must be called BEFORE the reverse-order CS walk (walk_node).
/// Populates `state.hash_records`, `state.hash_seen`, and `state.hash_keys`.
/// First occurrence wins — mirrors yanger_fxs's `add_hash` dict behavior.
///
/// `parent_path`: ancestor tagpath terms (excluding this node's name).
/// `enclosing_choice_name`: `Some(name)` when this node is a Case (passed from Choice handler).
/// `augment_ns`: if `Some((ns, ns_hash))`, this node is at the top level of an augmented subtree.
///   Use `[augmenting_ns|name]` ImproperList as its tagpath element. Children are called with
///   `None` so they use plain atoms (same namespace context as the augmenting module).
fn collect_hashes_forward(
    node: &SchemaNode,
    ns: &str,
    parent_path: &[Term],
    enclosing_choice_name: Option<&str>,
    augment_ns: Option<(&str, u128)>,
    ctx: &ExpansionCtx<'_>,
    state: &mut WalkState,
) {
    let name = &node.name;
    let name_bytes = name.as_bytes().to_vec();

    // Build this node's own tagpath element.
    let own_elem = match augment_ns {
        None => atom(name),
        Some((aug_ns, _)) => improper_list_pair(atom(aug_ns), atom(name)),
    };

    // Tagpath for this node: [own_elem, ...parent_path]
    let tagpath: Vec<Term> = std::iter::once(own_elem)
        .chain(parent_path.iter().cloned())
        .collect();

    // Register hash record for this node (first occurrence wins).
    if !state.hash_seen.contains(&name_bytes) {
        state.hash_seen.insert(name_bytes.clone());
        let hash_type = match &node.kind {
            SchemaNodeKind::Choice { .. } => {
                tuple(vec![atom("choice"), atom(name), tagpath_term(parent_path)])
            }
            SchemaNodeKind::Case { .. } => {
                let choice_name = enclosing_choice_name.unwrap_or(name);
                tuple(vec![
                    atom("case"),
                    atom(name),
                    atom(choice_name),
                    tagpath_term(parent_path),
                ])
            }
            _ => tuple(vec![atom("tagpath"), tagpath_term(&tagpath)]),
        };
        state.hash_records.push(make_hash_record(
            tuple(vec![atom("hash"), atom(name)]),
            hash_type,
            bigint(phash2_atom(name) as u128),
            int(0),
            atom("false"),
        ));
        state.hash_keys.push(name_bytes);
    }

    // Recurse into children in FORWARD (declaration) order.
    match &node.kind {
        SchemaNodeKind::Choice { .. } => {
            // Choice children are Case nodes; pass this choice's name as enclosing_choice_name.
            // Choices are transparent for tagpaths: children use the choice's parent_path.
            let children = node.children(ctx);
            for child in &children {
                collect_hashes_forward(child, ns, parent_path, Some(name), augment_ns, ctx, state);
            }
        }
        SchemaNodeKind::Case { .. } => {
            // Cases are transparent for tagpaths: children use the case's parent_path.
            let children = node.children(ctx);
            for child in &children {
                collect_hashes_forward(child, ns, parent_path, None, augment_ns, ctx, state);
            }
        }
        SchemaNodeKind::Rpc { .. } | SchemaNodeKind::Action { .. } => {
            // yanger_fxs: add_cs0/input processes input children, then add_cs0/output registers
            // '$output' FIRST (before output children). So insertion order: input → $output → output.
            let out_tagpath: Vec<Term> = {
                let mut tp = vec![atom("$output")];
                tp.extend_from_slice(&tagpath);
                tp
            };
            // Input children first.
            let input = node.input_children(ctx);
            for child in &input {
                collect_hashes_forward(child, ns, &tagpath, None, None, ctx, state);
            }
            // '$output' before output children (yanger registers it at start of add_cs0/output).
            let output_key = b"$output".to_vec();
            if !state.hash_seen.contains(&output_key) {
                state.hash_seen.insert(output_key.clone());
                state.hash_records.push(make_hash_record(
                    tuple(vec![atom("hash"), atom("$output")]),
                    tuple(vec![atom("pseudo_container"), tagpath_term(&out_tagpath)]),
                    bigint(phash2_atom("$output") as u128),
                    int(0),
                    atom("false"),
                ));
                state.hash_keys.push(output_key);
            }
            // Output children after '$output'.
            let output = node.output_children(ctx);
            for child in &output {
                collect_hashes_forward(child, ns, &out_tagpath, None, None, ctx, state);
            }
        }
        _ => {
            let children = node.children(ctx);
            for child in &children {
                // Children within the augmented container use plain atoms (same namespace context).
                collect_hashes_forward(child, ns, &tagpath, None, None, ctx, state);
            }
        }
    }
}

struct WalkState {
    cs_records: Vec<Term>,
    hash_records: Vec<Term>,
    /// Atom name bytes for each entry in `hash_records`, in walk order.
    /// Used to build the name → index map for reordering by yanger's insertion order.
    hash_keys: Vec<Vec<u8>>,
    /// Tracks which hash names have already been registered.
    /// First occurrence wins (case nodes win over same-named IS_CASE data nodes),
    /// mirroring yanger_fxs `add_hash` which ignores duplicate insertions.
    hash_seen: HashSet<Vec<u8>>,
    /// Unified Misc2 records: action records + node doc records, in reversed-DFS push order.
    /// Both actions and node docs are pushed during walk_node (reversed DFS), so they're
    /// naturally interleaved.  Written to Misc2 reversed so they appear in forward DFS order.
    misc_records: Vec<Term>,
    /// Names of `tailf:actionpoint` callpoints found in RPC/action nodes.
    actionpoint_names: Vec<String>,
    has_cdb: bool,
    has_cdb_oper: bool,
    max_keypath_length: u32,
    max_key_tuple_size: u32,
    /// Name of the module whose FXS file we are currently building.
    /// Used to suppress cross-module enumeration flags.
    file_module_name: String,
    /// Tracks generated t<hash> anonymous types for leaf-list and inline-constraint leaves.
    type_gen: TypeGen,
    /// Inherited hidden value from the parent node (mirrors yanger_fxs's S#state.hidden).
    /// When a node has no tailf:hidden of its own, it inherits this value.
    inherited_hidden: Term,
    /// Pre-built cache: module_name → namespace URI, for fast identityref base resolution.
    module_ns_cache: std::collections::HashMap<String, String>,
    /// Pre-computed set of deviate-not-supported paths across all modules.
    /// Computed once per FXS emission and reused for all augment handling.
    not_supported_paths: HashSet<Vec<(String, String)>>,
}

/// case_depth: -1 = not inside any case; 0 = direct child of a case;
///             N>0 = N NP-container levels deep inside a case.
/// enclosing_choice_name: Some(name) only when this node is a Case node
///   (passed from the Choice handler so Case can build the correct hash type).
/// optional_choice_ctx: mirrors S#state.optional_choice in yanger_fxs; true when
///   processing children inside an optional choice's cases.
/// augment_ns: `Some((ns, ns_hash))` when this node is at the top level of an augmented subtree
///   (i.e., directly placed into a different module's namespace). ns/ns_hash are the AUGMENTING
///   module's namespace. tagpath elements use `[augmenting_ns|name]` ImproperList encoding.
///   Children within the augmented subtree use `None` (plain atoms) once we enter the augmenting
///   module's namespace context, matching yanger's qtag(Ns, Name) where Ns == def_ns → plain int.
#[allow(clippy::too_many_arguments)]
fn walk_node(
    node: &SchemaNode,
    parent: Option<&SchemaNode>,
    ns: &str,
    ns_hash: u128,
    parent_path: &[Term],
    parent_keys: &[String],
    parent_mode: SubtreeMode,
    ancestor_list_count: u32,
    ancestor_has_keyless_list: bool,
    is_when_dependent: bool,
    case_depth: i32,
    enclosing_choice_name: Option<&str>,
    optional_choice_ctx: bool,
    augment_ns: Option<(&str, u128)>,
    module: &CompiledModule,
    ctx: &ExpansionCtx<'_>,
    state: &mut WalkState,
) {
    let name = &node.name;
    // Node's config attribute overrides parent mode (only for Config/Oper transitions)
    let mode = match (parent_mode, node.config) {
        (SubtreeMode::Config, Some(false)) => SubtreeMode::Oper,
        (SubtreeMode::Oper, Some(true)) => SubtreeMode::Config,
        _ => parent_mode,
    };
    let is_config = mode.is_config();
    let is_oper = mode.is_oper();

    // when-condition flags for this node.
    let has_when = !node.when.is_empty();
    let when_self_flags: u128 = (if has_when { F_CS_HAS_WHEN } else { 0 })
        | (if is_when_dependent {
            F_CS_IS_WHEN_DEPENDENT
        } else {
            0
        });

    // cli_flags: computed from tailf:cli-* extensions on this node.
    let (node_cli_flags, cli_words, cli_ext_extra) = compute_cli_flags(&node.extensions);

    // my_child_flags: flags this node contributes to its parent (from cli_flags bits).
    // These also get OR-ed into the node's own flags (yanger's add_child_info mechanism).
    let cli_child_flags = compute_cli_child_flags(&cli_words, node);

    // Build this node's own tagpath element.
    // For top-level augmented nodes: [AugmentingNs|name] (ImproperList), mirroring yanger
    // qtag(Ns, Name) when Ns != def_ns (the augmenting module's namespace differs from the
    // target context). For regular nodes or children within the augmented subtree: atom(name).
    let own_elem: Term = match augment_ns {
        None => atom(name),
        Some((aug_ns, _)) => improper_list_pair(atom(aug_ns), atom(name)),
    };

    // htag: for top-level augmented nodes → [AugNsHash|phash2(name)] (ImproperList), mirroring
    // yanger hqtag. For regular nodes or children within augmented subtree → bigint(phash2(name)).
    let htag: Term = match augment_ns {
        None => bigint(phash2_atom(name) as u128),
        Some((_, aug_ns_hash)) => {
            improper_list_pair(bigint(aug_ns_hash), bigint(phash2_atom(name) as u128))
        }
    };

    // tagpath for this node: [own_elem, parent, grandparent, ...]
    // Note: choice and case nodes are transparent — they do NOT contribute
    // to the tagpath. Their handlers pass `parent_path` unchanged to children.
    let mut tagpath: Vec<Term> = Vec::with_capacity(parent_path.len() + 1);
    tagpath.push(own_elem);
    tagpath.extend_from_slice(parent_path);

    // IKP depth = tagpath length + number of ancestor lists (each list adds a key tuple level).
    let ikp_depth = tagpath.len() as u32 + ancestor_list_count;

    // Flag to add to this node's cs flags when inside a keyless list subtree.
    let keyless_ancestor_flag: u128 = if ancestor_has_keyless_list {
        F_CS_ANCESTOR_HAS_KEYLESS_LIST
    } else {
        0
    };

    // IS_CASE flag: propagated to ALL nodes inside a case subtree.
    // case_depth encodes: depth in bits 0-29, default-case marker in bit 30.
    // -1 means "not inside any case".
    const CASE_DEFAULT_BIT: i32 = 1 << 30;
    let actual_depth = case_depth & !CASE_DEFAULT_BIT;
    let is_case_default = case_depth != -1 && (case_depth & CASE_DEFAULT_BIT) != 0;
    let is_case_flag: u128 = if actual_depth >= 0 {
        F_CS_IS_CASE
            | if is_case_default {
                F_CS_IS_CASE_DEFAULT
            } else {
                0
            }
    } else {
        0
    };

    // case_depth extra field: added to non-case-root nodes nested inside containers in a case.
    let case_depth_extra: Vec<Term> = if actual_depth > 0 {
        vec![tuple(vec![atom("case_depth"), int(actual_depth)])]
    } else {
        vec![]
    };

    // Compute effective hidden: inherit from parent if this node has no tailf:hidden.
    // Mirrors yanger_fxs get_hidden(Sn, OldHidden).
    let effective_hidden = get_hidden(&node.extensions, &state.inherited_hidden);
    let old_inherited_hidden = std::mem::replace(&mut state.inherited_hidden, effective_hidden.clone());

    // Hash records are pre-registered by collect_hashes_forward (Phase 1) in forward DFS order.
    // This CS walk (Phase 2) only generates cs records.

    match &node.kind {
        SchemaNodeKind::Container { presence, .. } => {
            let expanded = node.children(ctx);
            let exs_children = mk_exs_children_terms(&expanded, is_config, ctx);
            let is_presence = presence.is_some();
            // F_EXS_OPTIONAL_NP_CONTAINER is set when the NP container has no mandatory children,
            // mirroring yanger_fxs: `if Kind=='container', MinOccurs==1, not ChildIsMandatory`.
            // ChildIsMandatory depends on optional_choice_ctx (S#state.optional_choice in Erlang).
            let child_is_mand = !is_presence
                && expanded
                    .iter()
                    .any(|ch| child_is_mandatory(ch, optional_choice_ctx, mode, ctx));
            if std::env::var("YANGEST_DEBUG_NP").is_ok() && name.contains("ace-rule") {
                eprintln!(
                    "DEBUG ace-rule: name={name} opt_ctx={optional_choice_ctx} child_is_mand={child_is_mand} is_presence={is_presence} mode={mode:?}"
                );
                for ch in &expanded {
                    let r = child_is_mandatory(ch, optional_choice_ctx, mode, ctx);
                    eprintln!(
                        "  child {} ({:?}) -> {r}",
                        ch.name,
                        std::mem::discriminant(&ch.kind)
                    );
                }
            }
            if std::env::var("YANGEST_DEBUG_NP2").is_ok() && !is_presence && !child_is_mand {
            }
            if std::env::var("YANGEST_DEBUG_NP3").is_ok() && !is_presence && child_is_mand {
            }
            let base_exs_flags: u32 = if is_presence || child_is_mand {
                0
            } else {
                F_EXS_OPTIONAL_NP_CONTAINER
            };
            let exs_flags: u32 = base_exs_flags | if is_oper { F_EXS_READONLY } else { 0 };
            let min_occurs: i32 = if is_presence { 0 } else { 1 };

            let exs = make_exs(
                tagpath_term(&tagpath),
                undefined(),
                undefined(),
                undefined(),
                nil(),
                int(min_occurs),
                int(1),
                list(exs_children),
                uint(exs_flags),
                nil(),
            );

            let self_idx = reserve_slot(state);
            // Each entry is a range of cs_records indices from a direct child's subtree.
            // For Choice children (transparent), we include ALL records from the subtree
            // so child_aggregate can see mandatory leaves in any case branch.
            // For all other children, we include only the first record (the node itself).
            let mut child_ranges: Vec<std::ops::Range<usize>> = Vec::new();
            // Presence containers and non-presence containers have different when-barrier semantics:
            // presence containers are barriers (like lists), non-presence are not.
            let child_when_dep = if has_when {
                true
            } else if is_presence {
                false
            } else {
                is_when_dependent
            };
            // NP-containers inside a case increment case_depth for their children, preserving CASE_DEFAULT_BIT.
            // Presence containers (like lists) reset case_depth to -1 for children.
            let child_case_depth = if !is_presence && case_depth >= 0 {
                const CASE_DEFAULT_BIT: i32 = 1 << 30;
                (case_depth & !CASE_DEFAULT_BIT) + 1 | (case_depth & CASE_DEFAULT_BIT)
            } else {
                -1
            };
            for child in expanded.iter().rev() {
                let before = state.cs_records.len();
                walk_node(
                    child,
                    Some(node), // container is parent
                    ns,
                    ns_hash,
                    &tagpath,
                    &[],
                    mode,
                    ancestor_list_count,
                    ancestor_has_keyless_list,
                    child_when_dep,
                    child_case_depth,
                    None,
                    optional_choice_ctx,
                    None,
                    module,
                    ctx,
                    state,
                );
                let after = state.cs_records.len();
                if after > before {
                    // For transparent choice nodes, include the full subtree so that
                    // mandatory leaves inside any case branch contribute to child_aggregate.
                    let is_choice = matches!(child.kind, SchemaNodeKind::Choice { .. });
                    if is_choice {
                        child_ranges.push(before..after);
                    } else {
                        child_ranges.push(before..(before + 1));
                    }
                }
            }
            let immediate_child_cs: Vec<&Term> = child_ranges
                .iter()
                .flat_map(|r| r.clone().map(|i| &state.cs_records[i]))
                .collect();
            let (child_flags, all_cdb) = child_aggregate(&immediate_child_cs);

            let (flags, dbm, dba) = if is_oper {
                // CDB-operational container. F_CS_CHILD_READ_ONLY is the "self contribution":
                // yanger's child_flags/3 returns it for any READ-without-WRITE node, and
                // add_child_info sets it on the node itself (even with no children).
                let mut f = oper_rw_flags()
                    | F_CS_IS_CONTAINER
                    | F_CS_CHILD_READ_ONLY
                    | child_flags
                    | is_case_flag;
                if all_cdb {
                    f |= F_CS_CHILDREN_SAME_CONFIG_DB;
                }
                // Presence oper container contributes CHILD_OPTIONAL to parent (encoded here)
                if is_presence {
                    f |= F_CS_CHILD_OPTIONAL;
                }
                if has_mandatory_choice_child(&expanded) {
                    f |= F_CS_CHILD_MANDATORY_CHOICE;
                }
                state.has_cdb_oper = true;
                state.has_cdb = true;
                update_max_keypath(state, ikp_depth);
                (
                    f | keyless_ancestor_flag | when_self_flags,
                    atom("cdb"),
                    atom("volatile"),
                )
            } else if is_config {
                // CDB-config container
                let mut f = rw_flags(true)
                    | F_CS_IS_CONTAINER
                    | F_CS_NO_DEFAULTS
                    | child_flags
                    | is_case_flag;
                if all_cdb {
                    f |= F_CS_IS_CDB | F_CS_CHILDREN_SAME_CONFIG_DB;
                    state.has_cdb = true;
                }
                if is_presence {
                    // P-container self-seed: yanger_fxs seeds initial childflags with child_flags(self),
                    // which for a P-container returns F_CS_CHILD_OPTIONAL | F_CS_CHILD_DELETABLE (with F_CS_WRITE).
                    f |= F_CS_CHILD_OPTIONAL | F_CS_CHILD_DELETABLE;
                } else if optional_choice_ctx {
                    // NP-container inside an optional choice: its own entry can be deleted when the choice
                    // is deleted. yanger_fxs: OptionalChoice=true → F_CS_CHILD_DELETABLE self-seed.
                    f |= F_CS_CHILD_DELETABLE;
                }
                if has_mandatory_choice_child(&expanded) {
                    f |= F_CS_CHILD_MANDATORY_CHOICE;
                }
                update_max_keypath(state, ikp_depth);
                (
                    f | keyless_ancestor_flag | when_self_flags,
                    atom("cdb"),
                    nil(),
                )
            } else {
                // Notification/action param/result container — use mode's rw_flags for READ/IS_PARAM/IS_RESULT
                let mut f = mode.rw_flags()
                    | F_CS_IS_CONTAINER
                    | F_CS_CHILDREN_SAME_CONFIG_DB
                    | is_case_flag
                    | keyless_ancestor_flag
                    | when_self_flags;
                if has_mandatory_choice_child(&expanded) {
                    f |= F_CS_CHILD_MANDATORY_CHOICE;
                }
                (f, undefined(), nil())
            };
            let (node_hidden, mut node_meta, node_tailf_flags) =
                tailf_hidden_and_meta(&node.extensions);
            let (status_extra, status_flag) = yang_status_items(&node.status);
            node_meta.extend(status_extra);
            node_meta.extend(case_depth_extra);
            node_meta.extend(cli_ext_extra.clone());
            let extra_term = if node_meta.is_empty() {
                nil()
            } else {
                list(node_meta)
            };
            let doc_flag = doc_description_flag(&node.extensions, &node.description);
            if let Some(doc_term) = make_node_doc_term(&node.extensions, &tagpath) {
                state.misc_records.push(doc_term);
            }

            let cs = make_cs_node_with_hidden(
                ns,
                ns_hash,
                htag.clone(),
                exs,
                nil(),
                flags | status_flag | node_tailf_flags | cli_child_flags | doc_flag,
                dbm,
                dba,
                int(0),
                extra_term,
                effective_hidden.clone(),
                node_cli_flags,
            );
            state.cs_records[self_idx] = apply_when_must_to_cs(cs, node, parent, module, ctx);
        }

        SchemaNodeKind::Leaf {
            type_stmt,
            default,
            units,
            mandatory,
            musts,
            ..
        } => {
            let type_arg = type_stmt.arg.as_deref().unwrap_or("string");
            let tinfo = type_info_with_registry(type_arg, &node.module_name, ctx.registry);
            // Cross-module TYPEDEF nodes must not carry F_EXS_IS_ENUMERATION or a resolved
            // primitive_type — the runtime resolves the type via the defining module's
            // exs_type/load_type records.  For inline types (inline enum/bits/union), the
            // exs_type IS generated in the current file, so always emit the full info.
            let (prim_type, extra_flags) = if tinfo.is_typedef
                && tinfo
                    .typedef_defining_module
                    .as_deref()
                    .map(|m| m != state.file_module_name.as_str())
                    .unwrap_or(false)
            {
                use crate::types::F_EXS_IS_ENUMERATION;
                (
                    if tinfo.extra_exs_flags & F_EXS_IS_ENUMERATION != 0 {
                        crate::terms::undefined()
                    } else {
                        tinfo.primitive_type
                    },
                    tinfo.extra_exs_flags & !F_EXS_IS_ENUMERATION,
                )
            } else {
                (tinfo.primitive_type, tinfo.extra_exs_flags)
            };
            // 1-based key position, or 0 if not a key.
            // Keys apply in data trees and action/RPC inputs, but not in output/notification subtrees.
            // Mirrors yanger_fxs: sn_leaf_flags uses ?is_data_tree_or_input (NOT output, NOT notification).
            let raw_key_pos = parent_keys
                .iter()
                .position(|k| k == name)
                .map(|i| i + 1)
                .unwrap_or(0);
            let key_pos = if mode.is_data_tree_or_input() {
                raw_key_pos
            } else {
                0
            };
            let is_key = key_pos > 0;
            let is_mandatory = *mandatory;
            // yanger_fxs special case: mandatory empty-type config leaves without
            // tailf:cli-hide-in-submode are treated as optional (min_occurs=0).
            // get_mandatory_empty_leaf_vmfas adds a separate vmfa for the mandatory check.
            let has_cli_hide_in_submode = node
                .extensions
                .iter()
                .any(|e| e.module == "tailf-common" && e.name == "cli-hide-in-submode");
            let is_mandatory_empty_leaf_special_case = is_mandatory
                && is_config
                && type_arg == "empty"
                && !has_cli_hide_in_submode;
            // effective_min_occurs: for the empty-leaf special case, min_occurs=0.
            let effective_min_occurs: i32 = if is_key {
                1
            } else if is_mandatory && !is_mandatory_empty_leaf_special_case {
                1
            } else {
                0
            };
            // Load flags: add for non-IETF/builtin typedef from a different module.
            // Mirrors Erlang: {?INET,_}->0, {?YANG,_}->0, {?TAILF,_}->0, {?XS,_}->0,
            //                 ParentMRef->inherit, _ -> add GET_DEFAULT|SUPPRESS_ECHO|GET_TYPE_INFO
            // Condition covers two cases:
            //   1. Uses-expanded node from a foreign module (node.module_name != file_module)
            //   2. Local node with a cross-module typedef (e.g. cios-oper:notification-severity)
            let mut load_flags = if let Some(ref td_mod) = tinfo.typedef_defining_module {
                let is_builtin_mod = matches!(
                    td_mod.as_str(),
                    "ietf-inet-types" | "ietf-yang-types" | "tailf-common" | "tailf-inet-types"
                );
                let from_other = td_mod != &state.file_module_name;
                if !is_builtin_mod && (from_other || node.module_name != state.file_module_name) {
                    let mut f = F_LOAD_FXS_GET_SUPPRESS_ECHO | F_LOAD_FXS_GET_TYPE_INFO;
                    if !is_key && !is_mandatory {
                        // Enum typedef leaves with a default use PARSE_DEFAULT (load-time enum
                        // hash parsing) instead of GET_DEFAULT. This applies for both local enums
                        // (F_EXS_IS_ENUMERATION set) and cross-module enum typedefs (is_enum_base).
                        if (tinfo.extra_exs_flags & crate::types::F_EXS_IS_ENUMERATION != 0
                            || tinfo.is_enum_base)
                            && default.is_some()
                        {
                            f |= F_LOAD_FXS_PARSE_DEFAULT;
                        } else {
                            f |= F_LOAD_FXS_GET_DEFAULT;
                        }
                    }
                    f
                } else {
                    0
                }
            } else {
                0
            };
            // When load_flags == 0 but the leaf has a default value and the primitive type is
            // a "non-pre-computable" ConfD type (e.g. dateTime, inetAddressIPv4 — any type
            // whose base type is a string-restriction but whose primitive_type is not "string"),
            // yanger's mk_internal_value returns `undefined`, which triggers PARSE_DEFAULT.
            // We replicate that here: if the primitive type is not in the set of types that
            // can be stored as a pre-computed internal value, set F_LOAD_FXS_PARSE_DEFAULT.
            if load_flags == 0 && default.is_some() {
                if let Term::Atom(ref pt) = prim_type {
                    if !prim_type_is_precomputable(pt.name.as_str()) {
                        load_flags |= F_LOAD_FXS_PARSE_DEFAULT;
                    }
                }
            }
            let default_term = if let Some(dflt) = default.as_deref() {
                // For local enum types (load_flags == 0 and IS_ENUMERATION), encode the default
                // as {BENUMHASH=28, ordinal} matching yanger's mk_internal_value behavior.
                if load_flags == 0
                    && tinfo.extra_exs_flags & crate::types::F_EXS_IS_ENUMERATION != 0
                {
                    // For inline enumerations (type_arg == "enumeration"), scan type_stmt directly.
                    // For typedef-based enums, look up through the registry.
                    let ordinal = if type_arg == "enumeration" {
                        find_enum_ordinal_in_type(
                            type_stmt,
                            &node.module_name,
                            dflt,
                            ctx.registry,
                            1,
                        )
                    } else {
                        lookup_enum_ordinal(type_arg, &node.module_name, dflt, ctx.registry)
                    };
                    if let Some(ordinal) = ordinal {
                        tuple(vec![int(28), int(ordinal as i32)])
                    } else {
                        encode_internal_default(dflt, &prim_type, load_flags)
                    }
                } else {
                    encode_internal_default(dflt, &prim_type, load_flags)
                }
            } else {
                undefined()
            };
            let has_default = default_term != undefined();
            let (node_hidden, node_meta, node_tailf_flags) =
                tailf_hidden_and_meta(&node.extensions);
            let (extra_term, status_flag) = node_extra_with_meta_suffix(
                node_meta,
                cli_ext_extra.clone(),
                &node.status,
                units.as_deref(),
                load_flags,
            );
            let doc_flag = doc_description_flag(&node.extensions, &node.description);
            if let Some(doc_term) = make_node_doc_term(&node.extensions, &tagpath) {
                state.misc_records.push(doc_term);
            }

            // Detect inline type constraints (length, pattern, range) for t<hash> generation.
            // Types are always stored in the AUGMENTING/defining module's namespace, not the
            // target namespace. Use module.namespace (the FXS-file module's own namespace)
            // so that augmented-node types go into bgp.fxs's namespace, not native.fxs's.
            let (leaf_exs_type, prim_type_override) = if load_flags == 0 && !tinfo.is_typedef {
                maybe_generate_leaf_type(
                    &mut state.type_gen,
                    &module.namespace,
                    type_arg,
                    tinfo.exs_type.clone(),
                    type_stmt,
                    &node.module_name,
                    ctx.registry,
                    &state.module_ns_cache,
                    0, // direct leaf: use type-specific default (IS_ENUMERATION=2 / IS_BITS=64)
                    is_key || effective_min_occurs > 0,
                )
            } else {
                (tinfo.exs_type, None)
            };
            let effective_prim_type = prim_type_override.unwrap_or_else(|| prim_type.clone());

            let exs = make_exs(
                tagpath_term(&tagpath),
                leaf_exs_type,
                effective_prim_type,
                default_term,
                nil(),
                int(effective_min_occurs),
                int(1),
                nil(),
                uint(extra_flags | if is_oper { F_EXS_READONLY } else { 0 }),
                nil(),
            );

            let (flags, dbm, dba) = if is_oper {
                // CDB-operational leaf
                let mut f = oper_rw_flags()
                    | F_CS_CHILD_READ_ONLY
                    | F_CS_CHILDREN_SAME_CONFIG_DB
                    | is_case_flag;
                if is_key {
                    f |= F_CS_IS_KEY | F_CS_CHILD_MANDATORY_OR_DEFAULT;
                } else if effective_min_occurs > 0 {
                    // Non-key mandatory oper leaf: in yanger, child_flags/2 returns
                    // F_CS_CHILD_MANDATORY_OR_DEFAULT (MinOccurs > 0, not is_container).
                    // This gets added to the leaf's OWN flags via add_child_info
                    // (yanger adds MyChildFlags to Cs via add_child_info for leaves).
                    f |= F_CS_CHILD_MANDATORY_OR_DEFAULT;
                }
                state.has_cdb_oper = true;
                state.has_cdb = true;
                update_max_keypath(state, ikp_depth);
                (
                    f | status_flag | keyless_ancestor_flag | when_self_flags,
                    atom("cdb"),
                    atom("volatile"),
                )
            } else if is_config {
                // CDB-config leaf
                let mut f = rw_flags(true)
                    | F_CS_NO_DEFAULTS
                    | F_CS_IS_CDB
                    | F_CS_CHILDREN_SAME_CONFIG_DB
                    | F_CS_CHILD_READ_WRITE
                    | is_case_flag;
                if is_key {
                    f |= F_CS_IS_KEY | F_CS_CHILD_MANDATORY_OR_DEFAULT;
                } else {
                    // Compute optional/mandatory independently, mirroring Erlang child_flags/2
                    // which uses separate `bor` clauses for each flag group.
                    //
                    // is_optional_leaf = (effective_min_occurs == 0)
                    let is_optional_leaf = effective_min_occurs == 0;
                    // F_CS_CHILD_DEFAULT applies to any leaf with a default value, regardless of
                    // whether it is also mandatory. A mandatory leaf can have a default (used on
                    // create), and yanger_fxs always sets CHILD_DEFAULT in that case.
                    if has_default {
                        f |= F_CS_CHILD_DEFAULT;
                    }
                    if is_optional_leaf {
                        // is_optional_leaf AND is_write AND not is_key → DELETABLE + DEFAULT/OPTIONAL
                        f |= F_CS_CHILD_DELETABLE;
                        if !has_default {
                            f |= F_CS_CHILD_OPTIONAL;
                        }
                    } else if optional_choice_ctx {
                        // Mandatory leaf (not key) inside an optional choice: its containing choice
                        // branch can be deleted, making this leaf effectively deletable.
                        // yanger_fxs: OptionalChoice=true → F_CS_CHILD_DELETABLE for mandatory leaves.
                        f |= F_CS_CHILD_DELETABLE;
                    }
                    // MinOccurs > 0, not is_container → F_CS_CHILD_MANDATORY_OR_DEFAULT
                    // Also: has_default AND validatemfas != [] → F_CS_CHILD_MANDATORY_OR_DEFAULT
                    if effective_min_occurs > 0 || (has_default && !musts.is_empty()) {
                        f |= F_CS_CHILD_MANDATORY_OR_DEFAULT;
                    }
                }
                state.has_cdb = true;
                update_max_keypath(state, ikp_depth);
                (
                    f | status_flag | keyless_ancestor_flag | when_self_flags,
                    atom("cdb"),
                    nil(),
                )
            } else {
                // Notification/action param/result leaf
                let mut f = mode.rw_flags() | F_CS_CHILDREN_SAME_CONFIG_DB | is_case_flag;
                // Keys apply in action/RPC input (is_data_tree_or_input covers ActionInput).
                // Note: F_CS_CHILD_MANDATORY_OR_DEFAULT is NOT added for action input keys
                // (yanger_fxs sn_leaf_flags only returns F_CS_IS_KEY, not MANDATORY_OR_DEFAULT).
                if is_key {
                    f |= F_CS_IS_KEY;
                }
                (
                    f | status_flag | keyless_ancestor_flag | when_self_flags,
                    undefined(),
                    nil(),
                )
            };

            // Merge case_depth: insert after load_flags (if present) or at front.
            // Reference ordering: [load_flags?, case_depth, units?, ...]
            let extra_term = merge_case_depth(extra_term, case_depth_extra);

            let cs = make_cs_node_with_hidden(
                ns,
                ns_hash,
                htag.clone(),
                exs,
                nil(),
                flags | node_tailf_flags | cli_child_flags | doc_flag,
                dbm,
                dba,
                int(key_pos as i32),
                extra_term,
                effective_hidden.clone(),
                node_cli_flags,
            );
            let cs = apply_when_must_to_cs(cs, node, parent, module, ctx);
            state.cs_records.push(cs);
        }

        SchemaNodeKind::LeafList {
            type_stmt,
            min_elements,
            max_elements,
            units,
            ordered_by,
            ..
        } => {
            let type_arg = type_stmt.arg.as_deref().unwrap_or("string");
            let tinfo = type_info_with_registry(type_arg, &node.module_name, ctx.registry);
            // Compute AllLoadTypeFlags before tinfo is partially moved (for leaf-list thash).
            let ll_all_load_type_flags = compute_leaf_list_all_load_type_flags(
                type_arg,
                type_stmt,
                &tinfo,
                &node.module_name,
                ctx.registry,
            );
            let (base_prim_type, type_extra_flags) = if node.module_name != state.file_module_name {
                use crate::types::F_EXS_IS_ENUMERATION;
                (
                    if tinfo.extra_exs_flags & F_EXS_IS_ENUMERATION != 0 {
                        crate::terms::undefined()
                    } else {
                        tinfo.primitive_type
                    },
                    tinfo.extra_exs_flags & !F_EXS_IS_ENUMERATION,
                )
            } else {
                (tinfo.primitive_type, tinfo.extra_exs_flags)
            };
            // yanger wraps leaf-list primitive_type as {list, PrimType}
            let prim_type = match &base_prim_type {
                Term::Atom(_) => tuple(vec![atom("list"), base_prim_type]),
                _ => base_prim_type, // undefined or complex — leave as-is
            };
            let exs_flags =
                type_extra_flags | F_EXS_IS_LEAF_LIST | if is_oper { F_EXS_READONLY } else { 0 };
            let max_term = match max_elements {
                Some(n) => bigint(*n as u128),
                None => atom("unbounded"),
            };
            let ll_load_flags = if let Some(ref td_mod) = tinfo.typedef_defining_module {
                let is_builtin_mod = matches!(
                    td_mod.as_str(),
                    "ietf-inet-types" | "ietf-yang-types" | "tailf-common" | "tailf-inet-types"
                );
                let from_other = td_mod != &state.file_module_name;
                if !is_builtin_mod && (from_other || node.module_name != state.file_module_name) {
                    // Leaf-lists are never keys, so always get GET_DEFAULT.
                    F_LOAD_FXS_GET_DEFAULT | F_LOAD_FXS_GET_SUPPRESS_ECHO | F_LOAD_FXS_GET_TYPE_INFO
                } else {
                    0
                }
            } else {
                0
            };
            let (node_hidden, node_meta, node_tailf_flags) =
                tailf_hidden_and_meta(&node.extensions);
            let (extra_term, status_flag) = node_extra_with_meta_suffix(
                node_meta,
                cli_ext_extra.clone(),
                &node.status,
                units.as_deref(),
                ll_load_flags,
            );
            let doc_flag = doc_description_flag(&node.extensions, &node.description);
            if let Some(doc_term) = make_node_doc_term(&node.extensions, &tagpath) {
                state.misc_records.push(doc_term);
            }

            // Generate t<hash> list + unique_list types for this leaf-list.
            // For cross-module typedef leaf-lists, the base type ref comes from tinfo.exs_type.
            // For local/builtin leaf-lists, tinfo.exs_type is the base XSD/confd type.
            let leaf_list_exs_type = if ll_load_flags == 0 {
                // Local type: generate t<hash> types.
                // For inline enum/union/bits leaf-lists, generate the base type first.
                let base_type_ref = if !tinfo.is_typedef
                    && (type_arg == "enumeration" || type_arg == "union" || type_arg == "bits")
                {
                    maybe_generate_leaf_type(
                        &mut state.type_gen,
                        &module.namespace,
                        type_arg,
                        tinfo.exs_type,
                        type_stmt,
                        &node.module_name,
                        ctx.registry,
                        &state.module_ns_cache,
                        0, // direct leaf-list: use type-specific default
                        *min_elements > 0, // leaf-list can't be a key; mandatory if min > 0
                    )
                    .0
                } else {
                    tinfo.exs_type
                };
                let (base_ns, base_name) = exs_type_ns_name(&base_type_ref);
                let list_ref = state.type_gen.get_or_create_list_type(
                    &module.namespace,
                    base_ns,
                    base_name,
                    ll_all_load_type_flags,
                );
                let (list_ns, list_name) = exs_type_ns_name(&list_ref);
                let min = *min_elements;
                let max = *max_elements;
                state.type_gen.get_or_create_unique_list_type(
                    &module.namespace,
                    list_ns,
                    list_name,
                    min,
                    max,
                    ll_all_load_type_flags,
                )
            } else {
                // Cross-module typedef: use the base type directly (FXS loader handles it)
                tinfo.exs_type
            };

            let exs = make_exs(
                tagpath_term(&tagpath),
                leaf_list_exs_type,
                prim_type,
                undefined(),
                nil(),
                bigint(*min_elements as u128),
                max_term,
                nil(),
                uint(exs_flags),
                nil(),
            );

            let (flags, dbm, dba) = if is_oper {
                // Note: F_CS_CHILD_LIST is NOT set on the leaf-list itself; it's propagated
                // to the parent by child_aggregate when it sees F_CS_IS_LEAF_LIST.
                let mut f = oper_rw_flags()
                    | F_CS_IS_LEAF_LIST
                    | F_CS_CHILD_READ_ONLY
                    | F_CS_CHILDREN_SAME_CONFIG_DB
                    | is_case_flag;
                if *min_elements > 0 {
                    f |= F_CS_CHILD_MANDATORY_OR_DEFAULT;
                }
                state.has_cdb_oper = true;
                state.has_cdb = true;
                update_max_keypath(state, ikp_depth);
                (
                    f | status_flag | keyless_ancestor_flag | when_self_flags,
                    atom("cdb"),
                    atom("volatile"),
                )
            } else if is_config {
                let mut f = rw_flags(true)
                    | F_CS_IS_LEAF_LIST
                    | F_CS_NO_DEFAULTS
                    | F_CS_IS_CDB
                    | F_CS_CHILDREN_SAME_CONFIG_DB
                    | F_CS_CHILD_READ_WRITE
                    | F_CS_CHILD_DELETABLE
                    | is_case_flag;
                if *min_elements > 0 {
                    f |= F_CS_CHILD_MANDATORY_OR_DEFAULT;
                } else {
                    f |= F_CS_CHILD_OPTIONAL;
                }
                if matches!(ordered_by, OrderedBy::User) {
                    f |= F_CS_CHILD_ORDERED_BY;
                }
                state.has_cdb = true;
                update_max_keypath(state, ikp_depth);
                (
                    f | status_flag | keyless_ancestor_flag | when_self_flags,
                    atom("cdb"),
                    nil(),
                )
            } else {
                let mut f = mode.rw_flags()
                    | F_CS_IS_LEAF_LIST
                    | F_CS_CHILDREN_SAME_CONFIG_DB
                    | is_case_flag;
                if *min_elements > 0 {
                    f |= F_CS_CHILD_MANDATORY_OR_DEFAULT;
                }
                (
                    f | status_flag | keyless_ancestor_flag | when_self_flags,
                    undefined(),
                    nil(),
                )
            };

            // Merge case_depth: insert after load_flags (if present) or at front.
            let extra_term = merge_case_depth(extra_term, case_depth_extra);
            // cmp for leaf-list = sort order type (CS_CMP_USER=3 for ordered-by user, 0=normal)
            let cmp = sort_order_cmp(ordered_by, &node.extensions, mode);
            let cs = make_cs_node_with_hidden(
                ns,
                ns_hash,
                htag.clone(),
                exs,
                nil(),
                flags | node_tailf_flags | cli_child_flags | doc_flag,
                dbm,
                dba,
                cmp,
                extra_term,
                effective_hidden.clone(),
                node_cli_flags,
            );
            let cs = apply_when_must_to_cs(cs, node, parent, module, ctx);
            state.cs_records.push(cs);
        }

        SchemaNodeKind::List {
            key,
            min_elements,
            max_elements,
            ordered_by,
            ..
        } => {
            let expanded = node.children(ctx);
            // Key leaves must come first in children (mirrors sort_keys in yanger_fxs.erl).
            let exs_children: Vec<Term> = {
                let names: Vec<&str> = expanded.iter().map(|ch| ch.name.as_str()).collect();
                // Build name→term mapping (choice nodes get full choice encoding).
                let child_terms: Vec<(&str, Term)> = expanded
                    .iter()
                    .map(|ch| (ch.name.as_str(), mk_exs_child_term(ch, is_config, ctx)))
                    .collect();
                // Only reorder if keys are not already a prefix.
                if !key
                    .iter()
                    .map(|k| k.as_str())
                    .eq(names.iter().copied().take(key.len()))
                {
                    let key_set: std::collections::HashSet<&str> =
                        key.iter().map(|k| k.as_str()).collect();
                    let key_terms: Vec<Term> = key
                        .iter()
                        .filter_map(|k| {
                            child_terms
                                .iter()
                                .find(|(n, _)| *n == k.as_str())
                                .map(|(_, t)| t.clone())
                        })
                        .collect();
                    let rest_terms: Vec<Term> = child_terms
                        .iter()
                        .filter(|(n, _)| !key_set.contains(n))
                        .map(|(_, t)| t.clone())
                        .collect();
                    key_terms.into_iter().chain(rest_terms).collect()
                } else {
                    child_terms.into_iter().map(|(_, t)| t).collect()
                }
            };
            let max_term = match max_elements {
                Some(n) => bigint(*n as u128),
                None => atom("unbounded"),
            };

            let exs = make_exs(
                tagpath_term(&tagpath),
                undefined(),
                undefined(),
                undefined(),
                nil(),
                bigint(*min_elements as u128),
                max_term,
                list(exs_children),
                uint(if is_oper { F_EXS_READONLY } else { 0 }),
                nil(),
            );

            // A keyless list (no keys) that has no keyless ancestor is the "top keyless list".
            let is_top_keyless_list = key.is_empty() && !ancestor_has_keyless_list;

            let self_idx = reserve_slot(state);
            let mut immediate_child_indices: Vec<usize> = Vec::new();
            // Lists are barriers for when-dependency propagation; children only inherit
            // if the list itself has a when condition.
            let child_when_dep = has_when;
            for child in expanded.iter().rev() {
                let before = state.cs_records.len();
                walk_node(
                    child,
                    Some(node), // list is parent
                    ns,
                    ns_hash,
                    &tagpath,
                    key,
                    mode,
                    ancestor_list_count + 1,
                    ancestor_has_keyless_list || is_top_keyless_list,
                    child_when_dep,
                    -1,
                    None,
                    optional_choice_ctx,
                    None,
                    module,
                    ctx,
                    state,
                );
                if state.cs_records.len() > before {
                    immediate_child_indices.push(before);
                }
            }
            let immediate_child_cs: Vec<&Term> = immediate_child_indices
                .iter()
                .map(|&i| &state.cs_records[i])
                .collect();
            let (child_flags, all_cdb) = child_aggregate(&immediate_child_cs);

            // F_CS_HAS_PREFIX_LEAF: set on the list when any immediate child has tailf:cli-prefix-key.
            let has_prefix_leaf = expanded.iter().any(|ch| {
                ch.extensions.iter().any(|ext| {
                    ext.module == "tailf-common" && ext.name == "cli-prefix-key"
                })
            });
            let prefix_leaf_flag = if has_prefix_leaf { F_CS_HAS_PREFIX_LEAF } else { 0 };

            let key_count = key.len() as u32;
            // Only count keys in data subtrees (Config/Oper), not in action/notification subtrees.
            if mode.is_data_mode() && key_count > state.max_key_tuple_size {
                state.max_key_tuple_size = key_count;
            }

            let (flags, dbm, dba) = if is_oper {
                // A list contributes F_CS_CHILD_LIST + F_CS_CHILD_READ_ONLY to itself
                // (self-contribution in yanger via child_flags/3 for READ-without-WRITE nodes).
                let mut f = oper_rw_flags()
                    | F_CS_IS_LIST
                    | F_CS_CHILD_LIST
                    | F_CS_CHILD_READ_ONLY
                    | child_flags
                    | is_case_flag;
                if all_cdb {
                    f |= F_CS_CHILDREN_SAME_CONFIG_DB;
                }
                if is_top_keyless_list {
                    f |= F_CS_IS_TOP_KEYLESS_LIST;
                }
                if ancestor_has_keyless_list {
                    f |= F_CS_ANCESTOR_HAS_KEYLESS_LIST;
                }
                if has_mandatory_choice_child(&expanded) {
                    f |= F_CS_CHILD_MANDATORY_CHOICE;
                }
                state.has_cdb_oper = true;
                state.has_cdb = true;
                update_max_keypath(state, ikp_depth);
                (f | when_self_flags | prefix_leaf_flag, atom("cdb"), atom("volatile"))
            } else if is_config {
                let mut f = rw_flags(true)
                    | F_CS_IS_LIST
                    | F_CS_CHILD_LIST
                    // Self-seed: yanger_fxs seeds initial childflags with child_flags(self),
                    // which for a config list (with F_CS_WRITE) returns F_CS_CHILD_LIST | F_CS_CHILD_DELETABLE.
                    | F_CS_CHILD_DELETABLE
                    | F_CS_NO_DEFAULTS
                    | child_flags
                    | is_case_flag;
                if all_cdb {
                    f |= F_CS_IS_CDB | F_CS_CHILDREN_SAME_CONFIG_DB;
                    state.has_cdb = true;
                }
                if is_top_keyless_list {
                    f |= F_CS_IS_TOP_KEYLESS_LIST;
                }
                if ancestor_has_keyless_list {
                    f |= F_CS_ANCESTOR_HAS_KEYLESS_LIST;
                }
                if has_mandatory_choice_child(&expanded) {
                    f |= F_CS_CHILD_MANDATORY_CHOICE;
                }
                if matches!(ordered_by, OrderedBy::User) {
                    f |= F_CS_CHILD_ORDERED_BY;
                }
                update_max_keypath(state, ikp_depth);
                (f | when_self_flags | prefix_leaf_flag, atom("cdb"), nil())
            } else {
                let mut f =
                    mode.rw_flags() | F_CS_IS_LIST | F_CS_CHILDREN_SAME_CONFIG_DB | is_case_flag;
                if is_top_keyless_list {
                    f |= F_CS_IS_TOP_KEYLESS_LIST;
                }
                if ancestor_has_keyless_list {
                    f |= F_CS_ANCESTOR_HAS_KEYLESS_LIST;
                }
                if has_mandatory_choice_child(&expanded) {
                    f |= F_CS_CHILD_MANDATORY_CHOICE;
                }
                (f | when_self_flags | prefix_leaf_flag, undefined(), nil())
            };
            let (node_hidden, mut node_meta, node_tailf_flags) =
                tailf_hidden_and_meta(&node.extensions);
            let (status_extra, status_flag) = yang_status_items(&node.status);
            node_meta.extend(status_extra);
            node_meta.extend(case_depth_extra);
            node_meta.extend(cli_ext_extra.clone());
            let extra_term = if node_meta.is_empty() {
                nil()
            } else {
                list(node_meta)
            };
            let doc_flag = doc_description_flag(&node.extensions, &node.description);
            if let Some(doc_term) = make_node_doc_term(&node.extensions, &tagpath) {
                state.misc_records.push(doc_term);
            }

            let keys_term = list(key.iter().map(|k| atom(k)).collect());
            let cmp = sort_order_cmp(ordered_by, &node.extensions, mode);
            let cs = make_cs_node_with_hidden(
                ns,
                ns_hash,
                htag.clone(),
                exs,
                keys_term,
                flags | status_flag | node_tailf_flags | cli_child_flags | doc_flag,
                dbm,
                dba,
                cmp,
                extra_term,
                effective_hidden.clone(),
                node_cli_flags,
            );
            state.cs_records[self_idx] = apply_when_must_to_cs(cs, node, parent, module, ctx);
        }

        SchemaNodeKind::Choice { .. } => {
            // Choice nodes are transparent — no cs record.
            // Pass parent_path (NOT &tagpath) to preserve correct tagpath for children.
            // Pass the choice name so the Case handler can build its hash record correctly.
            // Set optional_choice_ctx for case children to is_optional_choice(this_choice),
            // mirroring S1 = S0#state{optional_choice = is_optional_choice(Sn)} in yanger_fxs.
            let child_when_dep = has_when || is_when_dependent;
            let child_opt_ctx = is_optional_choice(node);
            let cases: Vec<_> = node.children(ctx);
            // Detect default case: F_CS_IS_CASE_DEFAULT is set on children of the default case.
            let default_case_name: Option<&str> =
                if let SchemaNodeKind::Choice { default, .. } = &node.kind {
                    default.as_deref()
                } else {
                    None
                };
            for case_node in cases.iter().rev() {
                const CASE_DEFAULT_BIT: i32 = 1 << 30;
                let is_default = default_case_name
                    .map(|d| d == case_node.name.as_str())
                    .unwrap_or(false);
                // Encode "this case is the default" in CASE_DEFAULT_BIT passed to case handler.
                let case_signal = if is_default { CASE_DEFAULT_BIT } else { 0 };
                walk_node(
                    case_node,
                    parent, // choice is transparent; pass parent through
                    ns,
                    ns_hash,
                    parent_path,
                    &[],
                    mode,
                    ancestor_list_count,
                    ancestor_has_keyless_list,
                    child_when_dep,
                    case_signal,
                    Some(name),
                    child_opt_ctx,
                    augment_ns,
                    module,
                    ctx,
                    state,
                );
            }
        }

        SchemaNodeKind::Case { .. } => {
            // Case nodes are transparent — no cs record.
            // IS_CASE flag is propagated to all children by setting case_depth = 0.
            // Pass parent_path (NOT &tagpath) so children's tagpaths skip the case name.
            // CASE_DEFAULT_BIT in incoming case_depth signals "this case is the default case".
            let child_when_dep = has_when || is_when_dependent;
            let expanded = node.children(ctx);
            const CASE_DEFAULT_BIT: i32 = 1 << 30;
            let is_default_case = (case_depth & CASE_DEFAULT_BIT) != 0;
            let child_case_depth = if is_default_case { CASE_DEFAULT_BIT } else { 0 };
            for child in expanded.iter().rev() {
                walk_node(
                    child,
                    parent, // case is transparent; pass parent through
                    ns,
                    ns_hash,
                    parent_path,
                    &[],
                    mode,
                    ancestor_list_count,
                    ancestor_has_keyless_list,
                    child_when_dep,
                    child_case_depth,
                    None,
                    optional_choice_ctx,
                    augment_ns,
                    module,
                    ctx,
                    state,
                );
            }
        }

        SchemaNodeKind::Rpc { .. } | SchemaNodeKind::Action { .. } => {
            // Use the expansion-aware methods to handle Uses nodes in input/output.
            let expanded_input = node.input_children(ctx);
            let expanded_output = node.output_children(ctx);
            let has_input = !expanded_input.is_empty();
            let has_output = !expanded_output.is_empty();
            let is_std_action = matches!(node.kind, SchemaNodeKind::Action { .. });

            // Look for tailf:actionpoint extension to determine callback type.
            // Mirrors yanger_fxs mk_action logic:
            //   no actionpoint        → {erl, confd_internal, nyi}
            //   actionpoint + internal (+ cli-commit-prompt) → {erl, confd_internal, Id} (+ flag)
            //   actionpoint only      → {erl, capi, Id}
            enum ApCallback {
                Nyi,
                ConfInternal { name: String, cli_commit_prompt: bool },
                Capi { name: String },
            }
            let ap_callback: ApCallback = match node
                .extensions
                .iter()
                .find(|e| e.module == "tailf-common" && e.name == "actionpoint")
            {
                None => ApCallback::Nyi,
                Some(ap_ext) => {
                    let name = ap_ext.arg.clone().unwrap_or_default();
                    let has_internal = ap_ext.substmts.iter().any(|s| match &s.keyword {
                        Keyword::Extension { name: n, .. } => n == "internal",
                        Keyword::ExtensionPrefixed { name: n, .. } => n == "internal",
                        _ => false,
                    });
                    if has_internal {
                        let has_cli_commit = ap_ext.substmts.iter().any(|s| match &s.keyword {
                            Keyword::Extension { name: n, .. } => n == "cli-commit-prompt",
                            Keyword::ExtensionPrefixed { name: n, .. } => n == "cli-commit-prompt",
                            _ => false,
                        });
                        ApCallback::ConfInternal { name, cli_commit_prompt: has_cli_commit }
                    } else {
                        ApCallback::Capi { name }
                    }
                }
            };

            // Build and push the action record directly into misc_records (reversed DFS order).
            {
                const F_ACTION_HAS_CLI_COMMIT_PROMPT: u32 = 1 << 5;
                let mut flags: u32 = 0;
                if has_input { flags |= 1 << 0; }
                if has_output { flags |= 1 << 1; }
                if is_std_action { flags |= 1 << 4; }
                let callback = match &ap_callback {
                    ApCallback::Nyi =>
                        tuple(vec![atom("erl"), atom("confd_internal"), atom("nyi")]),
                    ApCallback::ConfInternal { name, cli_commit_prompt } => {
                        if *cli_commit_prompt { flags |= F_ACTION_HAS_CLI_COMMIT_PROMPT; }
                        tuple(vec![atom("erl"), atom("confd_internal"), atom(name)])
                    }
                    ApCallback::Capi { name } =>
                        tuple(vec![atom("erl"), atom("capi"), atom(name)]),
                };
                let action_term = tuple(vec![
                    atom("action"),
                    tuple(vec![atom("action"), list(tagpath.clone())]),
                    callback,
                    int(flags as i32),
                    undefined(),
                    undefined(),
                ]);
                state.misc_records.push(action_term);
                if let ApCallback::Capi { name } = &ap_callback {
                    state.actionpoint_names.push(name.clone());
                }
            }

            // Build exs.children list for the RPC/action node.
            // When output is non-empty: [...input_names, $output] (input first)
            // When output is empty: [$output, ...input_names] ($output first)
            // Action input children are not config, so is_config=false for choice encoding.
            let input_terms = mk_exs_children_terms(&expanded_input, false, ctx);
            let exs_children: Vec<Term> = if has_output {
                let mut v = input_terms;
                v.push(atom("$output"));
                v
            } else {
                let mut v = vec![atom("$output")];
                v.extend(input_terms);
                v
            };

            let exs = make_exs(
                tagpath_term(&tagpath),
                undefined(),
                undefined(),
                undefined(),
                nil(),
                int(1),
                int(1),
                list(exs_children),
                uint(0),
                nil(),
            );

            let self_idx = reserve_slot(state);
            let mut child_cs_indices: Vec<usize> = Vec::new();

            let out_tagpath: Vec<Term> = {
                let mut tp = vec![atom("$output")];
                tp.extend_from_slice(&tagpath);
                tp
            };
            let out_htag = bigint(phash2_atom("$output") as u128);
            let output_child_terms: Vec<Term> = mk_exs_children_terms(&expanded_output, false, ctx);
            let out_exs = make_exs(
                tagpath_term(&out_tagpath),
                undefined(),
                undefined(),
                undefined(),
                nil(),
                int(0),
                int(1),
                list(output_child_terms),
                uint(0),
                nil(),
            );
            // Note: '$output' hash record is registered in collect_hashes_forward (Phase 1).

            if has_output {
                // When the RPC has output: reserve $output slot first (lower index), then walk
                // output children, then walk input children (higher indices).
                // After fxs_write_list reversal: input leaves appear first in file, then output
                // children and $output, then the RPC itself.
                let out_self_idx = reserve_slot(state);
                for child in expanded_output.iter().rev() {
                    walk_node(
                        child,
                        Some(node), // RPC/action is parent
                        ns,
                        ns_hash,
                        &out_tagpath,
                        &[],
                        SubtreeMode::ActionOutput,
                        ancestor_list_count,
                        ancestor_has_keyless_list,
                        false,
                        -1,
                        None,
                        false,
                        None,
                        module,
                        ctx,
                        state,
                    );
                }
                let out_cs = make_cs_node(
                    ns,
                    ns_hash,
                    out_htag.clone(),
                    out_exs,
                    nil(),
                    0u128,
                    undefined(),
                    nil(),
                    int(0),
                    nil(),
                );
                state.cs_records[out_self_idx] = out_cs;
                child_cs_indices.push(out_self_idx);

                for child in expanded_input.iter().rev() {
                    let before = state.cs_records.len();
                    walk_node(
                        child,
                        Some(node), // RPC/action is parent
                        ns,
                        ns_hash,
                        &tagpath,
                        &[],
                        SubtreeMode::ActionInput,
                        ancestor_list_count,
                        ancestor_has_keyless_list,
                        false,
                        -1,
                        None,
                        false,
                        None,
                        module,
                        ctx,
                        state,
                    );
                    if state.cs_records.len() > before {
                        child_cs_indices.push(before);
                    }
                }
            } else {
                // When the RPC has no output: walk input children first (lower indices),
                // then place $output at a higher index.
                // After fxs_write_list reversal: $output appears first in file, then input leaves.
                for child in expanded_input.iter().rev() {
                    let before = state.cs_records.len();
                    walk_node(
                        child,
                        Some(node), // RPC/action is parent
                        ns,
                        ns_hash,
                        &tagpath,
                        &[],
                        SubtreeMode::ActionInput,
                        ancestor_list_count,
                        ancestor_has_keyless_list,
                        false,
                        -1,
                        None,
                        false,
                        None,
                        module,
                        ctx,
                        state,
                    );
                    if state.cs_records.len() > before {
                        child_cs_indices.push(before);
                    }
                }

                let out_self_idx = reserve_slot(state);
                let out_cs = make_cs_node(
                    ns,
                    ns_hash,
                    out_htag.clone(),
                    out_exs,
                    nil(),
                    0u128,
                    undefined(),
                    nil(),
                    int(0),
                    nil(),
                );
                state.cs_records[out_self_idx] = out_cs;
                child_cs_indices.push(out_self_idx);
            }

            // Aggregate child flags for the action node
            let child_cs: Vec<&Term> = child_cs_indices
                .iter()
                .map(|&i| &state.cs_records[i])
                .collect();
            let (child_flags, _all_cdb) = child_aggregate(&child_cs);

            let (node_hidden, mut node_meta, node_tailf_flags) =
                tailf_hidden_and_meta(&node.extensions);
            let (status_extra, status_flag) = yang_status_items(&node.status);
            node_meta.extend(status_extra);
            node_meta.extend(cli_ext_extra.clone());
            let extra_term = if node_meta.is_empty() {
                nil()
            } else {
                list(node_meta)
            };
            let doc_flag = doc_description_flag(&node.extensions, &node.description);
            if let Some(doc_term) = make_node_doc_term(&node.extensions, &tagpath) {
                state.misc_records.push(doc_term);
            }
            let mut flags = F_CS_IS_ACTION
                | F_CS_CHILDREN_SAME_CONFIG_DB
                | is_case_flag
                | status_flag
                | when_self_flags
                | node_tailf_flags
                | cli_child_flags
                | doc_flag;
            flags |= child_flags & F_CS_CHILD_MANDATORY_OR_DEFAULT;
            // Actions/RPCs always have exs.min_occurs=1, and yanger's child_info() feeds this
            // back into the action's own flags via add_child_info (the "self-reporting" pattern).
            flags |= F_CS_CHILD_MANDATORY_OR_DEFAULT;
            // Set F_CS_CHILD_MANDATORY_CHOICE if any input/output schema children include a
            // mandatory choice, OR if any child CS record (e.g. parameter list) has CHILD_MANDATORY_CHOICE
            // from its own children.
            if has_mandatory_choice_child(&expanded_input)
                || has_mandatory_choice_child(&expanded_output)
                || child_cs
                    .iter()
                    .any(|cs| extract_cs_flags(cs) & F_CS_CHILD_MANDATORY_CHOICE != 0)
            {
                flags |= F_CS_CHILD_MANDATORY_CHOICE;
            }
            let cs = make_cs_node_with_hidden(
                ns,
                ns_hash,
                htag.clone(),
                exs,
                nil(),
                flags,
                undefined(),
                nil(),
                int(0),
                extra_term,
                effective_hidden.clone(),
                node_cli_flags,
            );
            state.cs_records[self_idx] = apply_when_must_to_cs(cs, node, parent, module, ctx);
            // Count RPC/action node in max_keypath_length (like yanger does for all schema nodes).
            update_max_keypath(state, ikp_depth);
        }

        SchemaNodeKind::Notification { .. } => {
            let expanded = node.children(ctx);
            // Notification children are not config data.
            let exs_children = mk_exs_children_terms(&expanded, false, ctx);

            let exs = make_exs(
                tagpath_term(&tagpath),
                undefined(),
                undefined(),
                undefined(),
                nil(),
                int(0),
                int(1),
                list(exs_children),
                uint(0),
                nil(),
            );

            let self_idx = reserve_slot(state);
            let child_when_dep = has_when || is_when_dependent;
            for child in expanded.iter().rev() {
                walk_node(
                    child,
                    Some(node), // notification is parent
                    ns,
                    ns_hash,
                    &tagpath,
                    &[],
                    SubtreeMode::Notification,
                    ancestor_list_count,
                    ancestor_has_keyless_list,
                    child_when_dep,
                    -1,
                    None,
                    false,
                    None,
                    module,
                    ctx,
                    state,
                );
            }

            let (node_hidden, mut node_meta, node_tailf_flags) =
                tailf_hidden_and_meta(&node.extensions);
            let (status_extra, status_flag) = yang_status_items(&node.status);
            node_meta.extend(status_extra);
            node_meta.extend(cli_ext_extra.clone());
            let extra_term = if node_meta.is_empty() {
                nil()
            } else {
                list(node_meta)
            };
            let doc_flag = doc_description_flag(&node.extensions, &node.description);
            if let Some(doc_term) = make_node_doc_term(&node.extensions, &tagpath) {
                state.misc_records.push(doc_term);
            }
            let mut flags = F_CS_IS_NOTIF
                | is_case_flag
                | status_flag
                | when_self_flags
                | node_tailf_flags
                | cli_child_flags
                | doc_flag;
            if has_mandatory_choice_child(&expanded) {
                flags |= F_CS_CHILD_MANDATORY_CHOICE;
            }
            let cs = make_cs_node_with_hidden(
                ns,
                ns_hash,
                htag.clone(),
                exs,
                nil(),
                flags,
                undefined(),
                nil(),
                int(0),
                extra_term,
                effective_hidden.clone(),
                node_cli_flags,
            );
            state.cs_records[self_idx] = apply_when_must_to_cs(cs, node, parent, module, ctx);
        }

        SchemaNodeKind::AnyXml { mandatory, .. } | SchemaNodeKind::AnyData { mandatory, .. } => {
            let exs = make_exs(
                tagpath_term(&tagpath),
                undefined(),
                undefined(),
                undefined(),
                nil(),
                int(if *mandatory { 1 } else { 0 }),
                int(1),
                nil(),
                uint(0),
                nil(),
            );

            let (flags, dbm, dba) = if is_oper {
                let mut f = oper_rw_flags()
                    | F_CS_CHILD_READ_ONLY
                    | F_CS_CHILDREN_SAME_CONFIG_DB
                    | is_case_flag;
                if *mandatory {
                    f |= F_CS_CHILD_MANDATORY_OR_DEFAULT;
                }
                state.has_cdb_oper = true;
                state.has_cdb = true;
                update_max_keypath(state, ikp_depth);
                (
                    f | keyless_ancestor_flag | when_self_flags,
                    atom("cdb"),
                    atom("volatile"),
                )
            } else if is_config {
                let mut f = rw_flags(true)
                    | F_CS_NO_DEFAULTS
                    | F_CS_IS_CDB
                    | F_CS_CHILDREN_SAME_CONFIG_DB
                    | F_CS_CHILD_READ_WRITE
                    | F_CS_CHILD_DELETABLE
                    | is_case_flag;
                if !mandatory {
                    f |= F_CS_CHILD_OPTIONAL;
                }
                state.has_cdb = true;
                update_max_keypath(state, ikp_depth);
                (
                    f | keyless_ancestor_flag | when_self_flags,
                    atom("cdb"),
                    nil(),
                )
            } else {
                (
                    F_CS_CHILDREN_SAME_CONFIG_DB
                        | is_case_flag
                        | keyless_ancestor_flag
                        | when_self_flags,
                    undefined(),
                    nil(),
                )
            };
            let (node_hidden, mut node_meta, node_tailf_flags) =
                tailf_hidden_and_meta(&node.extensions);
            let (status_extra, status_flag) = yang_status_items(&node.status);
            node_meta.extend(status_extra);
            node_meta.extend(cli_ext_extra.clone());
            let extra_term = if node_meta.is_empty() {
                nil()
            } else {
                list(node_meta)
            };
            let doc_flag = doc_description_flag(&node.extensions, &node.description);
            if let Some(doc_term) = make_node_doc_term(&node.extensions, &tagpath) {
                state.misc_records.push(doc_term);
            }

            let cs = make_cs_node_with_hidden(
                ns,
                ns_hash,
                htag.clone(),
                exs,
                nil(),
                flags | status_flag | node_tailf_flags | cli_child_flags | doc_flag,
                dbm,
                dba,
                int(0),
                extra_term,
                effective_hidden.clone(),
                node_cli_flags,
            );
            let cs = apply_when_must_to_cs(cs, node, parent, module, ctx);
            state.cs_records.push(cs);
        }

        SchemaNodeKind::Uses { .. } => {
            // Expanded by parent's node.children(ctx); never reached directly.
        }
    }

    state.inherited_hidden = old_inherited_hidden;
}

// ---------------------------------------------------------------------------
// External augment walk
// ---------------------------------------------------------------------------

/// Walk all external augments in `module.augments`, generating:
/// - Hash records and type records for augmented nodes (Phase 1 pass)
/// - CS records for augmented nodes (Phase 2 pass, appended to state.cs_records)
/// - `load_augment` metadata records for the AugL section
///
/// Mirrors `yanger_fxs:mk_augment_cs/5` and the AugL section build in fill_tab.
fn walk_augments(
    module: &CompiledModule,
    ctx: &ExpansionCtx<'_>,
    state: &mut WalkState,
) -> Vec<Term> {
    use yangest_core::compiler::AugmentEntry;

    // Fast path: skip modules that have no augments.
    if module.augments.is_empty() {
        return vec![];
    }

    // Use pre-computed deviate-not-supported paths from WalkState (computed once, not per-module).
    let not_supported = &state.not_supported_paths;

    // Pre-compute data for each augment (skip local/empty ones).
    struct AugmentData {
        enabled_nodes: Vec<SchemaNode>,
        target_ns: String,
        target_ns_hash: u128,
        target_tagpath_terms: Vec<Term>,
        target_htag: Term,
        target_type_term: Term,
        children_terms: Vec<Term>,
        aug_ns_pair: (String, u128),
        target_module_arc: Option<Arc<CompiledModule>>,
        target_path: Vec<yangest_core::compiler::PathStep>,
        /// SubtreeMode for augmented nodes, derived from target node's config setting.
        target_mode: SubtreeMode,
    }
    let augment_data: Vec<Option<AugmentData>> = module
        .augments
        .iter()
        .map(|augment| {
            let last_step_prefix =
                augment.target_path.last().and_then(|s| s.prefix.as_deref());
            let is_local_augment = match last_step_prefix {
                None => true,
                Some(pfx) => pfx == module.prefix.as_str(),
            };
            if is_local_augment {
                return None;
            }

            let target_module_arc: Option<Arc<CompiledModule>> = augment
                .target_path
                .first()
                .and_then(|step| step.prefix.as_deref())
                .and_then(|pfx| {
                    module.prefix_map.get(pfx).or_else(|| {
                        if module.prefix == pfx {
                            Some(&module.key.name)
                        } else {
                            None
                        }
                    })
                })
                .and_then(|tmod_name| ctx.registry.resolve_import(tmod_name, None));
            let empty_overlay = NodeOverlayMap::new();
            let target_overlay: &NodeOverlayMap = target_module_arc
                .as_deref()
                .map(|m| &m.overlay)
                .unwrap_or(&empty_overlay);

            let expanded_nodes: Vec<SchemaNode> = expand_children(
                &augment.nodes,
                &module.prefix,
                &module.key.name,
                target_overlay,
                &augment.target_path,
                ctx,
            );

            let enabled_nodes: Vec<SchemaNode> = if not_supported.is_empty() {
                expanded_nodes
            } else {
                let normalized_target =
                    normalize_augment_path(augment, module).unwrap_or_default();
                expanded_nodes
                    .into_iter()
                    .filter(|child| {
                        let mut child_path = normalized_target.clone();
                        child_path.push((child.module_name.clone(), child.name.clone()));
                        let keep = !not_supported.contains(&child_path);
                        keep
                    })
                    .collect()
            };

            if enabled_nodes.is_empty() {
                return None;
            }

            let (target_ns, target_ns_hash) =
                resolve_augment_target_ns(augment, module, ctx.registry)?;

            // Walk the target path, skipping structural nodes (choice, case, input, output),
            // to compute the correct target_tagpath, target_htag and target_type.
            // In yanger_fxs, find_target() does NOT update ParentCs for structural nodes,
            // so target_htag = last REAL data node's htag, not the literal last step's name.
            let (target_tagpath_terms, target_htag, target_type_term) =
                compute_augment_target_info(
                    &augment.target_path,
                    module,
                    target_module_arc.as_deref(),
                    ctx,
                );

            let augmenting_ns = &module.namespace;
            let children_terms: Vec<Term> = enabled_nodes
                .iter()
                .map(|node| improper_list_pair(atom(augmenting_ns), atom(&node.name)))
                .collect();

            let augmenting_ns_hash = phash2_atom(augmenting_ns) as u128;

            // Determine the subtree mode for augmented nodes based on the target node's config.
            // If the target node has `config false`, augmented nodes inherit that setting.
            // We walk the target path tracking the *effective* config (last explicit value wins),
            // because intermediate/leaf nodes may have config=None (inherited).
            // Use the ctx-aware version to handle openconfig-style grouping expansion.
            // Pass `module` as source so cross-module prefixes in the path can be resolved.
            let target_mode = target_module_arc.as_deref().map(|tmod| find_effective_config_at_path_ctx(&augment.target_path, tmod, module, ctx))
                .map(|eff_config| if eff_config { SubtreeMode::Config } else { SubtreeMode::Oper })
                .unwrap_or(SubtreeMode::Config);

            Some(AugmentData {
                enabled_nodes,
                target_ns,
                target_ns_hash,
                target_tagpath_terms,
                target_htag,
                target_type_term,
                children_terms,
                aug_ns_pair: (module.namespace.clone(), augmenting_ns_hash),
                target_module_arc,
                target_path: augment.target_path.clone(),
                target_mode,
            })
        })
        .collect();

    // Phase 1: hash + type collection in REVERSE augment order.
    // yanger_fxs uses lists:foldr over augments, which processes the LAST augment first.
    // When the same leaf name appears in multiple augments, the last augment's location
    // wins for hash registration (first-occurrence-wins in hash_seen with reverse iteration).
    for data in augment_data.iter().rev().flatten() {
        let aug_ns_pair = Some((data.aug_ns_pair.0.as_str(), data.aug_ns_pair.1));
        let aug_parent_path: &Vec<Term> = &data.target_tagpath_terms;
        for node in data.enabled_nodes.iter() {
            collect_types_forward(
                node,
                &module.namespace,
                &module.key.name,
                ctx,
                &mut state.type_gen,
                &state.module_ns_cache,
            );
            collect_hashes_forward(
                node,
                &data.target_ns,
                aug_parent_path,
                None,
                aug_ns_pair,
                ctx,
                state,
            );
        }
    }

    // Phase 2: CS record walk and load_augment record construction in FORWARD augment order.
    let mut la_records: Vec<Term> = Vec::new();

    for data in augment_data.iter().flatten() {
        let aug_ns_pair = Some((data.aug_ns_pair.0.as_str(), data.aug_ns_pair.1));
        let aug_parent_path: &Vec<Term> = &data.target_tagpath_terms;

        // Compute is_when_dependent for the augmented children.
        // Augmented nodes land inside the target container; if the target container
        // is in a when-dependent context (e.g. its parent has HAS_WHEN), the augmented
        // nodes must also have F_CS_IS_WHEN_DEPENDENT.
        let aug_is_when_dependent = data
            .target_module_arc
            .as_deref()
            .map(|tmod| find_child_when_dep_at_path(&data.target_path, tmod, module, ctx))
            .unwrap_or(false);

        let saved_max_kp = state.max_keypath_length;
        state.max_keypath_length = 0;
        let saved_has_cdb = state.has_cdb;
        let saved_has_cdb_oper = state.has_cdb_oper;

        let mut direct_child_cs_indices: Vec<usize> = Vec::new();
        for node in data.enabled_nodes.iter().rev() {
            let child_self_idx = state.cs_records.len();
            direct_child_cs_indices.push(child_self_idx);
            walk_node(
                node,
                None, // augmented top-level: no parent known
                &data.target_ns,
                data.target_ns_hash,
                aug_parent_path,
                &[],
                data.target_mode,
                0,
                false,
                aug_is_when_dependent,
                -1,
                None,
                false,
                aug_ns_pair,
                module,
                ctx,
                state,
            );
        }

        state.has_cdb = saved_has_cdb;
        state.has_cdb_oper = saved_has_cdb_oper;

        let keypath_length = state
            .max_keypath_length
            .saturating_sub(aug_parent_path.len() as u32);
        state.max_keypath_length = saved_max_kp;

        let propagate_flags_up: u128 = {
            let child_cs: Vec<&Term> = direct_child_cs_indices
                .iter()
                .filter_map(|&idx| state.cs_records.get(idx))
                .collect();
            let (flags, _) = child_aggregate(&child_cs);
            flags
        };

        let (required_set_flags, required_clr_flags) = compute_required_flags(
            &data.target_path,
            data.target_module_arc.as_deref(),
            data.target_mode,
            ctx,
        );

        la_records.push(make_load_augment(
            list(aug_parent_path.clone()),
            data.target_htag.clone(),
            atom(&data.target_ns),
            bigint(data.target_ns_hash),
            data.target_type_term.clone(),
            list(data.children_terms.clone()),
            nil(), // actions
            nil(), // notifs
            int(keypath_length as i32),
            bigint(required_set_flags),
            bigint(required_clr_flags),
            bigint(propagate_flags_up),
            nil(), // propagate_extra_up
            int(0),
        ));
    }

    la_records
}

/// Compute required_set_flags and required_clr_flags for a load_augment record.
///
/// Mirrors yanger_fxs:
///   ReqSetF = TargetCs2.flags & ~non_required_set_flags()
///   ReqClrF = required_clr_flags() & ~TargetCs2.flags
///
/// For data nodes (containers and lists), the result simplifies to:
///   Config: required_set_flags = F_CS_READ | F_CS_WRITE | kind_flag, required_clr_flags = F_CS_IS_PARAM | F_CS_IS_RESULT
///   Oper:   required_set_flags = F_CS_READ | kind_flag,               required_clr_flags = F_CS_WRITE | F_CS_IS_PARAM | F_CS_IS_RESULT
fn compute_required_flags(
    target_path: &[PathStep],
    target_module: Option<&CompiledModule>,
    target_mode: SubtreeMode,
    ctx: &ExpansionCtx<'_>,
) -> (u128, u128) {
    let Some(tmod) = target_module else {
        return default_required_flags();
    };

    // Try raw traversal first (fast path — handles most cases).
    // Fall back to Uses-expanded traversal if raw misses (e.g. path goes through a grouping).
    let node = find_schema_node_at_path_raw(target_path, tmod)
        .or_else(|| find_schema_node_at_path_ctx(target_path, tmod, ctx));

    let node = if let Some(n) = node {
        n
    } else {
        // The target node wasn't found in the root module's schema tree.
        // This happens when part of the path crosses into another module's augmented subtree.
        // Try to find the terminal node via its defining module (from the last step's prefix).
        let found = find_cross_module_target_node(target_path, tmod, ctx);
        match found {
            Some(n) => n,
            None => {
                // Node not found but we still know target_mode — use it with container default.
                let effective_config = target_mode == SubtreeMode::Config;
                let kind_flag = F_CS_IS_CONTAINER;
                return if effective_config {
                    (F_CS_READ | F_CS_WRITE | kind_flag, F_CS_IS_PARAM | F_CS_IS_RESULT)
                } else {
                    (F_CS_READ | kind_flag, F_CS_WRITE | F_CS_IS_PARAM | F_CS_IS_RESULT)
                };
            }
        }
    };

    let effective_config = target_mode == SubtreeMode::Config;
    kind_based_required_flags(&node, effective_config)
}

fn default_required_flags() -> (u128, u128) {
    // Default: assume container target.
    // This is the most common case; list targets are rare in this corpus.
    let kind_flag = F_CS_IS_CONTAINER;
    let required_set_flags = F_CS_READ | F_CS_WRITE | kind_flag;
    let required_clr_flags = F_CS_IS_PARAM | F_CS_IS_RESULT;
    (required_set_flags, required_clr_flags)
}

fn kind_based_required_flags(node: &SchemaNode, effective_config: bool) -> (u128, u128) {
    const ALL_REQ_CLR: u128 = F_CS_READ | F_CS_WRITE | F_CS_IS_PARAM | F_CS_IS_RESULT;
    let kind_flag: u128 = match &node.kind {
        SchemaNodeKind::List { .. } => F_CS_IS_LIST,
        SchemaNodeKind::Container { .. } => F_CS_IS_CONTAINER,
        _ => 0,
    };
    if effective_config {
        // Config target: READ + WRITE both required.
        let required_set_flags = F_CS_READ | F_CS_WRITE | kind_flag;
        let required_clr_flags = ALL_REQ_CLR & !(F_CS_READ | F_CS_WRITE);
        (required_set_flags, required_clr_flags)
    } else {
        // Oper target: WRITE_OPERATIONAL not WRITE; must clear WRITE.
        // ReqSetF = F_CS_READ | kind_flag  (WRITE is absent from oper flags)
        // ReqClrF = F_CS_WRITE | F_CS_IS_PARAM | F_CS_IS_RESULT
        let required_set_flags = F_CS_READ | kind_flag;
        let required_clr_flags = ALL_REQ_CLR & !F_CS_READ;
        (required_set_flags, required_clr_flags)
    }
}

/// Try to find the target node when the path crosses module boundaries.
/// When a path step has a prefix that resolves to a different module, search that
/// module's augments and children for the node.
fn find_cross_module_target_node(
    target_path: &[PathStep],
    root_module: &CompiledModule,
    ctx: &ExpansionCtx<'_>,
) -> Option<SchemaNode> {
    // Walk the path; when we can't find a step in the current children, switch to the module
    // that the step's prefix resolves to and search its augment entries.
    let module_of_step = |step: &PathStep| -> Option<Arc<CompiledModule>> {
        let pfx = step.prefix.as_deref()?;
        let mod_name = if pfx == root_module.prefix.as_str() {
            &root_module.key.name
        } else {
            root_module.prefix_map.get(pfx)?
        };
        ctx.registry.resolve_import(mod_name, None)
    };

    // Try to find the terminal node by searching from the last cross-module step.
    // Walk backwards from the last step to find the deepest cross-module transition.
    let last_step = target_path.last()?;
    let defining_module = module_of_step(last_step)?;

    // Search the defining module's augments for a node matching the last step name.
    let terminal_name = &last_step.name;
    for aug in &defining_module.augments {
        for node in &aug.nodes {
            if &node.name == terminal_name {
                return Some(node.clone());
            }
            // Also look one level deep (e.g., the node might be inside an augmented container).
            if let Some(children) = node_raw_children(node) {
                for child in children {
                    if &child.name == terminal_name {
                        return Some(child.clone());
                    }
                }
            }
        }
    }

    // Also search the defining module's own top-level children.
    for node in &defining_module.children {
        if &node.name == terminal_name {
            return Some(node.clone());
        }
    }

    None
}

fn node_raw_children(node: &SchemaNode) -> Option<&Vec<SchemaNode>> {
    match &node.kind {
        SchemaNodeKind::Container { children, .. } => Some(children),
        SchemaNodeKind::List { children, .. } => Some(children),
        SchemaNodeKind::Case { children, .. } => Some(children),
        _ => None,
    }
}

/// Walk a target module's schema tree following the given path steps (by unqualified name).
/// Returns the SchemaNode at the last path step, or None if not found.
/// Uses RAW (unexpanded) children to avoid expensive uses-expansion for large modules like native.
fn find_schema_node_at_path(
    target_path: &[PathStep],
    module: &CompiledModule,
) -> Option<SchemaNode> {
    find_schema_node_at_path_raw(target_path, module)
}

/// Walk target_path through module's raw children and return the effective
/// config value — i.e. the last explicit `config` declaration encountered.
/// Returns `true` (config) if no explicit value is found anywhere in the path.
fn find_effective_config_at_path(target_path: &[PathStep], module: &CompiledModule) -> bool {
    let mut current: &[SchemaNode] = &module.children;
    let mut effective_config: bool = true; // default: config

    for step in target_path {
        let name = &step.name;
        let mut matched = None;
        for node in current {
            if &node.name == name {
                matched = Some(node);
                break;
            }
        }
        match matched {
            None => {
                return effective_config;
            }
            Some(node) => {
                if let Some(cfg) = node.config {
                    effective_config = cfg;
                }
                current = raw_children(&node.kind);
            }
        }
    }

    effective_config
}

/// Like find_effective_config_at_path but uses the expansion context to expand
/// grouping uses — required for openconfig-style modules where `state` containers
/// and their `config false` are inside groupings.
///
/// Also handles cross-module steps: when a path step has a prefix from a different
/// module (e.g. `/oc-if:interfaces/oc-if:interface/oc-if-eth:ethernet/oc-if-eth:state`),
/// the function looks up that module's augments to continue the walk.
///
/// `source_module` is the module that defines the augment statement — its prefix_map
/// is used to resolve cross-module prefixes in the target path steps.
fn find_effective_config_at_path_ctx(
    target_path: &[PathStep],
    module: &CompiledModule,
    source_module: &CompiledModule,
    ctx: &ExpansionCtx<'_>,
) -> bool {
    if target_path.is_empty() {
        return true;
    }

    let mut effective_config = true;
    let first = target_path.first().unwrap();

    let Some(mut node) = module.find_child(&first.name, ctx) else {
        // Fall back to raw traversal if ctx lookup misses.
        return find_effective_config_at_path(target_path, module);
    };

    if let Some(cfg) = node.config {
        effective_config = cfg;
    }

    for (i, step) in target_path[1..].iter().enumerate() {
        match node.find_child(&step.name, ctx) {
            Some(n) => {
                if let Some(cfg) = n.config {
                    effective_config = cfg;
                }
                node = n;
            }
            None => {
                // Not found via direct traversal — may be a cross-module augmented node.
                // Use source_module's prefix_map since path prefixes are from the augment author.
                if let Some(step_pfx) = step.prefix.as_deref() {
                    let step_mod = source_module
                        .prefix_map
                        .get(step_pfx)
                        .and_then(|name| ctx.registry.resolve_import(name, None));
                    if let Some(step_mod_arc) = step_mod {
                        // Find this module's augment targeting target_path[0..=i].
                        // walked_path covers steps 0..=i (i.e. first + i more steps).
                        let walked_path = &target_path[0..=i]; // steps 0..i inclusive
                        if let Some(augment_node) = find_cross_module_step(
                            &step_mod_arc,
                            walked_path,
                            &step.name,
                            ctx,
                        ) {
                            if let Some(cfg) = augment_node.config {
                                effective_config = cfg;
                            }
                            node = augment_node;
                            continue;
                        }
                    }
                }
                break;
            }
        }
    }

    effective_config
}

/// Look for a named node inside a module's augments that target `walked_path`.
/// Used when a path step crosses into a node augmented by `step_module`.
fn find_cross_module_step(
    step_module: &CompiledModule,
    walked_path: &[PathStep],
    step_name: &str,
    ctx: &ExpansionCtx<'_>,
) -> Option<SchemaNode> {
    for augment in &step_module.augments {
        if augment_targets_path(&augment.target_path, walked_path, step_module) {
            // The augment targets our current position — look for step_name in its nodes.
            let empty_overlay = NodeOverlayMap::new();
            if let Some(found) = find_child_in_raw(step_name, &augment.nodes, &empty_overlay, ctx) {
                return Some(found);
            }
        }
    }
    None
}

/// Check whether an augment's target_path matches the walked_path.
/// We match by local name only (prefixes may differ between modules).
fn augment_targets_path(
    augment_target: &[PathStep],
    walked_path: &[PathStep],
    _step_module: &CompiledModule,
) -> bool {
    if augment_target.len() != walked_path.len() {
        return false;
    }
    augment_target
        .iter()
        .zip(walked_path.iter())
        .all(|(a, w)| a.name == w.name)
}

/// Compute whether the CHILDREN of the target node at `target_path` should have
/// `F_CS_IS_WHEN_DEPENDENT` set.  This is used when augmenting into a container
/// that lives inside a `when`-constrained parent — the augmented leaves must
/// inherit the same `IS_WHEN_DEPENDENT` the target container would propagate.
///
/// Uses raw (unexpanded) traversal to avoid expensive grouping expansion.  If a
/// step is only reachable through a `uses` grouping we fall back on the ctx-based
/// variant for that single step.
///
/// Mirrors the `child_when_dep` propagation rules in `walk_node`:
///   - list resets to its own `has_when`
///   - presence container clears (= false)
///   - NP container (and choice/case) inherit: `has_when || is_when_dependent`
fn find_child_when_dep_at_path(
    target_path: &[PathStep],
    module: &CompiledModule,
    source_module: &CompiledModule,
    ctx: &ExpansionCtx<'_>,
) -> bool {
    if target_path.is_empty() {
        return false;
    }

    // First, try a fast raw-only traversal (no grouping expansion).  This handles
    // the common case where all steps are directly defined schema nodes.
    if let Some(result) = find_child_when_dep_raw(&module.children, target_path) {
        return result;
    }

    // Fall back to ctx-based traversal (which expands Uses nodes) for paths that
    // cross through groupings or cross-module augments.
    find_child_when_dep_ctx(target_path, module, source_module, ctx)
}

/// Raw (no grouping expansion) variant of `find_child_when_dep_at_path`.
/// Returns `Some(result)` if the full path was found, `None` if any step was missed.
fn find_child_when_dep_raw(mut current: &[SchemaNode], target_path: &[PathStep]) -> Option<bool> {
    let mut child_when_dep = false;

    for step in target_path {
        let node = current.iter().find(|n| n.name == step.name)?;
        child_when_dep = compute_node_child_when_dep(node, child_when_dep);
        current = raw_children(&node.kind);
    }

    Some(child_when_dep)
}

/// Full ctx-based variant with grouping expansion and cross-module fallback.
fn find_child_when_dep_ctx(
    target_path: &[PathStep],
    module: &CompiledModule,
    source_module: &CompiledModule,
    ctx: &ExpansionCtx<'_>,
) -> bool {
    let first = target_path.first().unwrap();
    let Some(mut node) = module.find_child(&first.name, ctx) else {
        return false;
    };

    let mut child_when_dep = compute_node_child_when_dep(&node, false);

    for (i, step) in target_path[1..].iter().enumerate() {
        match node.find_child(&step.name, ctx) {
            Some(n) => {
                child_when_dep = compute_node_child_when_dep(&n, child_when_dep);
                node = n;
            }
            None => {
                if let Some(step_pfx) = step.prefix.as_deref() {
                    let step_mod = source_module
                        .prefix_map
                        .get(step_pfx)
                        .and_then(|name| ctx.registry.resolve_import(name, None));
                    if let Some(step_mod_arc) = step_mod {
                        let walked_path = &target_path[0..=i];
                        if let Some(augment_node) =
                            find_cross_module_step(&step_mod_arc, walked_path, &step.name, ctx)
                        {
                            child_when_dep =
                                compute_node_child_when_dep(&augment_node, child_when_dep);
                            node = augment_node;
                            continue;
                        }
                    }
                }
                break;
            }
        }
    }

    child_when_dep
}

/// Given a node and the `is_when_dependent` value it itself has (inherited from its parent),
/// compute what `child_when_dep` value its children should get.
fn compute_node_child_when_dep(node: &SchemaNode, is_when_dependent: bool) -> bool {
    use yangest_core::compiler::SchemaNodeKind;
    let has_when = !node.when.is_empty();
    match &node.kind {
        SchemaNodeKind::List { .. } => has_when,
        SchemaNodeKind::Container { presence, .. } => {
            if has_when {
                true
            } else if presence.is_some() {
                false
            } else {
                is_when_dependent
            }
        }
        // Choice, case, etc. are transparent: propagate like NP container.
        _ => has_when || is_when_dependent,
    }
}

fn find_schema_node_at_path_ctx(
    target_path: &[PathStep],
    module: &CompiledModule,
    ctx: &ExpansionCtx<'_>,
) -> Option<SchemaNode> {
    if target_path.is_empty() {
        return None;
    }

    // Use early-termination find_child at each level to avoid materializing all children.
    let first = target_path.first()?;
    let mut current = module.find_child(&first.name, ctx)?;

    for step in &target_path[1..] {
        current = current.find_child(&step.name, ctx)?;
    }

    Some(current)
}

fn find_schema_node_at_path_raw(
    target_path: &[PathStep],
    module: &CompiledModule,
) -> Option<SchemaNode> {
    if target_path.is_empty() {
        return None;
    }

    let mut current: &[SchemaNode] = &module.children;
    let mut found: Option<&SchemaNode> = None;

    for step in target_path {
        let name = &step.name;
        let mut matched = None;
        for node in current {
            if &node.name == name {
                matched = Some(node);
                break;
            }
        }
        match matched {
            None => return None,
            Some(node) => {
                current = raw_children(&node.kind);
                found = Some(node);
            }
        }
    }

    found.cloned()
}
/// Get raw (unexpanded) children from a SchemaNodeKind without triggering uses-expansion.
fn raw_children(kind: &SchemaNodeKind) -> &[SchemaNode] {
    match kind {
        SchemaNodeKind::Container { children, .. } => children,
        SchemaNodeKind::List { children, .. } => children,
        SchemaNodeKind::Choice { cases, .. } => cases,
        SchemaNodeKind::Case { children, .. } => children,
        SchemaNodeKind::Rpc { input, .. } => input,
        SchemaNodeKind::Action { input, .. } => input,
        SchemaNodeKind::Notification { children, .. } => children,
        _ => &[],
    }
}

/// Resolve the target namespace for an augment entry.
///
/// Uses the last step's prefix to find the target module, then returns
/// (namespace_string, phash2_of_namespace).
fn resolve_augment_target_ns(
    augment: &yangest_core::compiler::AugmentEntry,
    module: &CompiledModule,
    registry: &yangest_core::compiler::ModuleRegistry,
) -> Option<(String, u128)> {
    // Use the FIRST step's prefix — the root/target module that owns the augment path.
    // This mirrors yanger's TargetModuleName which is the root of the augmented path.
    // E.g. for `/ios:native/ios:route-map/ios-route-map:set`, root = ios:native → native.
    let first_step = augment.target_path.first()?;
    let target_ns = if let Some(ref prefix) = first_step.prefix {
        // Look up prefix in this module's prefix_map to get the imported module name.
        let target_module_name = module.prefix_map.get(prefix).or_else(|| {
            if module.prefix == *prefix {
                Some(&module.key.name)
            } else {
                None
            }
        });
        let target_module_name = target_module_name?;
        let target_module = registry.resolve_import(target_module_name, None);
        target_module?.namespace.clone()
    } else {
        // No prefix → same module's namespace (self-augment).
        module.namespace.clone()
    };
    let target_ns_hash = phash2_atom(&target_ns) as u128;
    Some((target_ns, target_ns_hash))
}

/// Build a target_tagpath for a load_augment record.
///
/// Returns the list elements in innermost-first order (ready to pass to `list()`).
/// Path steps that cross a module boundary are encoded as ImproperList `[ns|name]`;
/// steps within the same module as the previous step are plain atoms.
///
/// This mirrors yanger_fxs's `get_qinfo` logic: augmented-in nodes stored as
/// `{module, name}` tuples in the target schema tree get namespace-qualified tags.
fn build_target_tagpath_terms(
    target_path: &[yangest_core::compiler::PathStep],
    module: &CompiledModule,
    registry: &yangest_core::compiler::ModuleRegistry,
) -> Vec<Term> {
    if target_path.is_empty() {
        return Vec::new();
    }
    // Resolve prefix → module name using the augmenting module's imports.
    let resolve_module = |pfx: Option<&str>| -> Option<String> {
        match pfx {
            None => None,
            Some(p) => {
                if p == module.prefix.as_str() {
                    Some(module.key.name.clone())
                } else {
                    module.prefix_map.get(p).cloned()
                }
            }
        }
    };
    // Seed current_module from the first step (the target/root module).
    let mut current_module: Option<String> =
        resolve_module(target_path[0].prefix.as_deref());
    let mut terms: Vec<Term> = Vec::with_capacity(target_path.len());
    for step in target_path {
        let step_module = resolve_module(step.prefix.as_deref());
        let same_module = match (&step_module, &current_module) {
            (Some(sm), Some(cm)) => sm == cm,
            // No prefix → treat as same module (local/unqualified reference)
            (None, _) => true,
            (Some(_), None) => false,
        };
        if same_module {
            terms.push(atom(&step.name));
        } else {
            // Crossing a module boundary: encode as [ns|name] ImproperList.
            let step_ns = step_module
                .as_deref()
                .and_then(|mn| registry.resolve_import(mn, None))
                .map(|m| m.namespace.clone())
                .unwrap_or_default();
            terms.push(improper_list_pair(atom(&step_ns), atom(&step.name)));
            current_module = step_module;
        }
    }
    // Reverse to get innermost-first ordering expected by load_augment.target_tagpath.
    terms.reverse();
    terms
}

/// Compute target_tagpath terms, target_htag and target_type for a load_augment record.
///
/// Mirrors yanger_fxs `find_target` + `skip_tagpath_nodes` + `target_type`:
///   - Structural nodes (choice, case) and virtual RPC pseudo-nodes (input/output) do NOT
///     update the "effective" node for htag/tagpath purposes.
///   - target_tagpath = tagpath of the last REAL (CS-generating) node.
///   - target_htag    = htag of that last real node (ImproperList for cross-module boundary).
///   - target_type    = encoding of the actual terminal node's kind (choice/case/input/etc).
///
/// Fast path: paths without "input"/"output" steps have no virtual RPC nodes and no
/// choice/case targets, so tagpath/htag can be computed from the path steps alone without
/// schema-tree navigation.  Schema navigation is only performed for paths that contain an
/// "input" or "output" step, i.e. those augmenting into RPC/Action bodies.
///
/// Returns `(tagpath_terms_innermost_first, target_htag, target_type)`.
fn compute_augment_target_info(
    target_path: &[yangest_core::compiler::PathStep],
    module: &CompiledModule,
    target_module: Option<&CompiledModule>,
    ctx: &ExpansionCtx<'_>,
) -> (Vec<Term>, Term, Term) {
    use yangest_core::compiler::SchemaNodeKind;

    if target_path.is_empty() {
        return (vec![], undefined(), undefined());
    }

    // Resolve a step's prefix to a module name using the augmenting module's imports.
    let resolve_mod_name = |pfx: Option<&str>| -> Option<String> {
        pfx.and_then(|p| {
            if p == module.prefix.as_str() {
                Some(module.key.name.clone())
            } else {
                module.prefix_map.get(p).cloned()
            }
        })
    };

    // --- Fast path ---------------------------------------------------
    // For paths without "input"/"output" steps, all steps correspond to real schema nodes
    // (no virtual RPC pseudo-nodes), and no choice/case target is expected.
    // Tagpath and htag can be derived directly from the path steps and their prefixes,
    // without any schema-tree traversal.
    let needs_navigation = target_path
        .iter()
        .any(|s| s.name == "input" || s.name == "output");

    if !needs_navigation {
        let mut real_module_name: Option<String> =
            resolve_mod_name(target_path[0].prefix.as_deref());
        let mut terms: Vec<Term> = Vec::with_capacity(target_path.len());
        let mut last_crosses = false;
        let mut last_ns_str = String::new();
        let mut last_name = String::new();

        for step in target_path.iter() {
            let step_mod = resolve_mod_name(step.prefix.as_deref());
            let crosses = module_names_differ(&step_mod, &real_module_name);
            let ns_str = if crosses {
                step_mod
                    .as_deref()
                    .and_then(|mn| ctx.registry.resolve_import(mn, None))
                    .map(|m| m.namespace.clone())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let term = if crosses {
                improper_list_pair(atom(&ns_str), atom(&step.name))
            } else {
                atom(&step.name)
            };
            terms.push(term);
            last_crosses = crosses;
            last_ns_str = ns_str;
            last_name = step.name.clone();
            if step_mod.is_some() {
                real_module_name = step_mod;
            }
        }

        let target_htag = if last_crosses {
            let ns_hash = phash2_atom(&last_ns_str) as u128;
            let name_hash = phash2_atom(&last_name) as u128;
            improper_list_pair(bigint(ns_hash), bigint(name_hash))
        } else {
            bigint(phash2_atom(&last_name) as u128)
        };

        terms.reverse();
        return (terms, target_htag, undefined());
    }
    // --- End fast path -----------------------------------------------

    // Track module transitions for tagpath encoding.
    let mut current_module_name: Option<String> =
        resolve_mod_name(target_path[0].prefix.as_deref());
    // Module name of the most recently seen real node.
    let mut real_module_name: Option<String> = current_module_name.clone();

    // Tagpath terms for real nodes only.
    let mut real_terms: Vec<Term> = Vec::new();
    // Last real node name and whether its term was a cross-module ImproperList (for htag).
    let mut last_real_name: String = String::new();
    let mut last_real_crosses: bool = false;
    let mut last_real_ns_str: String = String::new();

    // Structural ancestors from the first structural node onwards (for target_type).
    let mut structural_kinds: Vec<bool> = Vec::new(); // true = choice, false = case
    let mut structural_names: Vec<String> = Vec::new();

    // Navigation state: use targeted early-termination search via `find_child`.
    // This expands Uses groupings lazily (only the specific child we need) rather than
    // allocating a full Vec of all children at each step — crucial for large modules
    // like Cisco-IOS-XE-native which have deep grouping hierarchies.
    let mut current_node: Option<SchemaNode> = None; // None = search module root
    let mut rpc_use_output = false; // if true, next step searches RPC output, not input
    let mut prev_was_rpc = false;

    for step in target_path.iter() {
        let name = &step.name;

        // Find the target child using early-termination search.
        let found: Option<SchemaNode> = if let Some(ref parent) = current_node {
            if rpc_use_output {
                rpc_use_output = false;
                parent.find_output_child(name, ctx)
            } else if prev_was_rpc {
                parent.find_input_child(name, ctx)
            } else {
                parent.find_child(name, ctx)
            }
        } else {
            // First step: search module root.
            target_module.and_then(|tm| tm.find_child(name, ctx))
        };

        match found {
            None => {
                // Node not found in the target schema tree.
                // Detect virtual RPC input/output pseudo-node (structural, not in schema).
                let is_rpc_io = prev_was_rpc && (name == "input" || name == "output");
                if is_rpc_io {
                    // Virtual structural node — skip term/htag updates; continue walking.
                    // Keep `prev_was_rpc = true` so the NEXT step still calls find_input_child
                    // (or find_output_child via rpc_use_output).  current_node stays as the Rpc.
                    if name == "output" {
                        rpc_use_output = true;
                    }
                    // prev_was_rpc intentionally left TRUE here.
                    continue;
                }
                // Real node not found (cross-module augmented or uses-expanded leaf).
                let step_mod = resolve_mod_name(step.prefix.as_deref());
                let crosses = module_names_differ(&step_mod, &real_module_name);
                let ns_str = if crosses {
                    step_mod
                        .as_deref()
                        .and_then(|mn| ctx.registry.resolve_import(mn, None))
                        .map(|m| m.namespace.clone())
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let term = if crosses {
                    improper_list_pair(atom(&ns_str), atom(name))
                } else {
                    atom(name)
                };
                real_terms.push(term);
                last_real_name = name.clone();
                last_real_crosses = crosses;
                last_real_ns_str = ns_str;
                real_module_name = step_mod.or_else(|| current_module_name.clone());
                structural_kinds.clear();
                structural_names.clear();
                prev_was_rpc = false;
                break;
            }
            Some(node) => {
                prev_was_rpc = matches!(
                    node.kind,
                    SchemaNodeKind::Rpc { .. } | SchemaNodeKind::Action { .. }
                );
                let is_structural = matches!(
                    node.kind,
                    SchemaNodeKind::Choice { .. } | SchemaNodeKind::Case { .. }
                );

                let step_mod = resolve_mod_name(step.prefix.as_deref());
                if module_names_differ(&step_mod, &current_module_name) {
                    current_module_name = step_mod.clone();
                }

                if is_structural {
                    let is_choice = matches!(node.kind, SchemaNodeKind::Choice { .. });
                    structural_kinds.push(is_choice);
                    structural_names.push(node.name.clone());
                } else {
                    let crosses = module_names_differ(&step_mod, &real_module_name);
                    let ns_str = if crosses {
                        step_mod
                            .as_deref()
                            .and_then(|mn| ctx.registry.resolve_import(mn, None))
                            .map(|m| m.namespace.clone())
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    let term = if crosses {
                        improper_list_pair(atom(&ns_str), atom(name))
                    } else {
                        atom(name)
                    };
                    real_terms.push(term);
                    last_real_name = name.clone();
                    last_real_crosses = crosses;
                    last_real_ns_str = ns_str;
                    real_module_name = step_mod.or_else(|| current_module_name.clone());
                    structural_kinds.clear();
                    structural_names.clear();
                }

                // Store found node as the new parent for the next step.
                current_node = Some(node);
            }
        }
    }

    // Compute target_type from structural suffix.
    let target_type_term = if structural_kinds.is_empty() {
        undefined()
    } else {
        // structural_names are in path order (first structural node first).
        // choice_path in yanger_fxs: outermost choice/case first.
        let choice_names: Vec<Term> = structural_names.iter().map(|n| atom(n)).collect();
        if structural_kinds[0] {
            tuple(vec![atom("choice"), list(choice_names)])
        } else {
            tuple(vec![atom("case"), list(choice_names)])
        }
    };

    // Compute target_htag from the last real node.
    let target_htag = if last_real_name.is_empty() {
        // No real node found; fall back to the last path step with boundary detection.
        // (happens for local augments which should have been filtered earlier)
        let last = target_path.last().unwrap();
        bigint(phash2_atom(&last.name) as u128)
    } else if last_real_crosses {
        let ns_hash = phash2_atom(&last_real_ns_str) as u128;
        let name_hash = phash2_atom(&last_real_name) as u128;
        improper_list_pair(bigint(ns_hash), bigint(name_hash))
    } else {
        bigint(phash2_atom(&last_real_name) as u128)
    };

    real_terms.reverse();
    (real_terms, target_htag, target_type_term)
}

/// Returns true if two module name options are both Some and differ.
#[inline]
fn module_names_differ(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x != y,
        _ => false,
    }
}

/// Resolve an identityref `base` argument to (namespace, local_name).
///
/// `base_ref` is the raw YANG string, e.g. `"direction"` or `"ios:direction"`.
/// `node_module_name` is the name of the module containing the type statement.
/// `module_ns_cache` maps module_name → namespace URI for O(1) lookup.
fn resolve_identity_base_to_ns_name(
    base_ref: &str,
    node_module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
    module_ns_cache: &std::collections::HashMap<String, String>,
) -> Option<(String, String)> {
    let (prefix_opt, local_name) = if let Some(colon) = base_ref.find(':') {
        (Some(&base_ref[..colon]), &base_ref[colon + 1..])
    } else {
        (None, base_ref)
    };

    // Find the namespace of the module that defines this identity.
    let ns = if let Some(prefix) = prefix_opt {
        // Resolve prefix → module_name via the node's module prefix_map.
        let node_module = registry.resolve_import(node_module_name, None)?;
        let defining_module_name = node_module.prefix_map.get(prefix)?;
        module_ns_cache.get(defining_module_name.as_str())?.clone()
    } else {
        module_ns_cache.get(node_module_name)?.clone()
    };

    Some((ns, local_name.to_owned()))
}

/// Collect all `deviate not-supported` paths from all modules in the registry.
///
/// Returns a HashSet where each entry is a normalized path as a Vec of
/// `(module_name, node_name)` pairs. Module names are resolved using each
/// module's own prefix_map.
///
/// This is needed to detect when an augment's children have been deviated
/// not-supported by a cross-module deviation — a case that yangest's
/// `apply_deviations` skips (see `deviation_leaf_targets_module` in compile.rs).
fn collect_deviate_not_supported_paths(
    registry: &yangest_core::compiler::ModuleRegistry,
) -> HashSet<Vec<(String, String)>> {
    use yangest_core::ast::BuiltInKeyword;

    let mut result = HashSet::new();

    for module in registry.modules.values() {
        let own_prefix = &module.prefix;
        let own_name = &module.key.name;

        for dev_stmt in module.stmt.get_substmts(BuiltInKeyword::Deviation) {
            // Check if any sub-statement is `deviate not-supported`
            let has_not_supported = dev_stmt.substmts.iter().any(|s| {
                s.keyword.is_builtin(BuiltInKeyword::Deviate)
                    && s.arg.as_deref() == Some("not-supported")
            });
            if !has_not_supported {
                continue;
            }

            let Some(path_str) = dev_stmt.arg.as_deref() else {
                continue;
            };
            let trimmed = path_str.trim_start_matches('/');
            if trimmed.is_empty() {
                continue;
            }

            let mut normalized: Vec<(String, String)> = Vec::new();
            let mut ok = true;
            for step in trimmed.split('/') {
                if step.is_empty() {
                    continue;
                }
                let (prefix_opt, name) = if let Some(colon) = step.find(':') {
                    (Some(&step[..colon]), &step[colon + 1..])
                } else {
                    (None, step)
                };
                let module_name = if let Some(pfx) = prefix_opt {
                    if pfx == own_prefix {
                        own_name.clone()
                    } else if let Some(mn) = module.prefix_map.get(pfx) {
                        mn.clone()
                    } else {
                        ok = false;
                        break;
                    }
                } else {
                    own_name.clone()
                };
                normalized.push((module_name, name.to_string()));
            }
            if ok && !normalized.is_empty() {
                result.insert(normalized);
            }
        }
    }

    result
}

/// Normalize an augment target path from prefix-notation to (module_name, node_name) pairs.
/// Uses the augmenting module's prefix_map to resolve prefixes.
fn normalize_augment_path(
    augment: &yangest_core::compiler::AugmentEntry,
    module: &CompiledModule,
) -> Option<Vec<(String, String)>> {
    let mut result = Vec::new();
    for step in &augment.target_path {
        let module_name = if let Some(ref pfx) = step.prefix {
            if pfx == &module.prefix {
                module.key.name.clone()
            } else {
                module.prefix_map.get(pfx)?.clone()
            }
        } else {
            module.key.name.clone()
        };
        result.push((module_name, step.name.clone()));
    }
    Some(result)
}

/// Check if an augment's target path is navigable through the target module's schema,
/// i.e., all nodes in the path exist and are feature-enabled.
///
/// Mirrors yanger_fxs `find_target` which returns `not_found` when a path node
/// has `if_feature_result = false`. Uses targeted per-step expansion to avoid
/// expanding the entire target module's schema tree.
fn is_augment_target_path_navigable(
    augment: &yangest_core::compiler::AugmentEntry,
    module: &CompiledModule,
    registry: &yangest_core::compiler::ModuleRegistry,
    ctx: &ExpansionCtx<'_>,
) -> bool {
    // Determine the root (target) module from the first path step.
    let Some(first_step) = augment.target_path.first() else {
        return false;
    };
    let target_module_name = if let Some(ref prefix) = first_step.prefix {
        if let Some(name) = module.prefix_map.get(prefix).or_else(|| {
            if module.prefix == *prefix {
                Some(&module.key.name)
            } else {
                None
            }
        }) {
            name.clone()
        } else {
            return false;
        }
    } else {
        module.key.name.clone()
    };
    let Some(target_module) = registry.resolve_import(&target_module_name, None) else {
        return false;
    };

    // Navigate through the path steps using targeted per-step search.
    // We expand only the Uses nodes we encounter, not the entire module tree.
    let mut current_raw: &[SchemaNode] = &target_module.children;
    // Need an owned Vec for non-root steps
    let mut owned_children: Vec<SchemaNode>;

    for step in &augment.target_path {
        match find_node_in_raw(step.name.as_str(), current_raw, &target_module.overlay, ctx) {
            Some(found) => {
                owned_children = found.children(ctx);
                current_raw = &[];
                // We can't directly borrow owned_children as current_raw in the loop,
                // so we need a different approach — see below.
                let _ = owned_children; // silence unused warning
                // Workaround: use a stack-based approach outside this function.
                return is_augment_path_navigable_step(
                    &augment.target_path[1..],
                    found,
                    &target_module.overlay,
                    ctx,
                );
            }
            None => return false,
        }
    }
    true
}

/// Recursively navigate path steps starting from a found node.
fn is_augment_path_navigable_step(
    remaining: &[yangest_core::compiler::PathStep],
    current_node: SchemaNode,
    overlay: &yangest_core::compiler::NodeOverlayMap,
    ctx: &ExpansionCtx<'_>,
) -> bool {
    if remaining.is_empty() {
        return true;
    }
    let children = current_node.children(ctx);
    let step = &remaining[0];
    let found = find_node_in_vec(step.name.as_str(), &children, overlay, ctx);
    match found {
        Some(node) => is_augment_path_navigable_step(&remaining[1..], node, overlay, ctx),
        None => false,
    }
}

/// Fast (non-expanding) check: does the augment target path exist in the compiled schema?
/// Returns false only if the target is DEFINITELY absent — i.e., no direct child or Uses
/// node at each step could contain the named node. Returns true if uncertain (conservative).
/// This avoids the expensive full Uses expansion of is_augment_target_path_navigable.
fn augment_target_exists_raw(
    augment: &yangest_core::compiler::AugmentEntry,
    module: &CompiledModule,
    registry: &yangest_core::compiler::ModuleRegistry,
    ctx: &ExpansionCtx<'_>,
) -> bool {
    let Some(first_step) = augment.target_path.first() else {
        return false;
    };
    let target_module_name = if let Some(ref prefix) = first_step.prefix {
        let Some(name) = module.prefix_map.get(prefix).or_else(|| {
            if module.prefix == *prefix {
                Some(&module.key.name)
            } else {
                None
            }
        }) else {
            return false;
        };
        name.clone()
    } else {
        module.key.name.clone()
    };
    let Some(target_module) = registry.resolve_import(&target_module_name, None) else {
        return false;
    };

    check_path_raw(&augment.target_path, &target_module.children)
}

/// Recursively check if a path exists in the raw (non-uses-expanded) children slice.
/// Returns true conservatively if any Uses node is encountered (cannot inspect without expansion).
fn check_path_raw(path: &[yangest_core::compiler::PathStep], children: &[SchemaNode]) -> bool {
    let Some(step) = path.first() else {
        return true; // empty path = done
    };
    let name = step.name.as_str();
    let rest = &path[1..];
    let mut has_uses = false;

    for child in children {
        match &child.kind {
            SchemaNodeKind::Uses { .. } => {
                has_uses = true;
            }
            _ => {
                if child.name == name {
                    if rest.is_empty() {
                        return true;
                    }
                    return match child.raw_children() {
                        Some(sub) => check_path_raw(rest, sub),
                        None => true, // leaf/leaflist but path continues — conservative
                    };
                }
            }
        }
    }
    // Not found directly; be conservative if Uses nodes were present.
    has_uses
}

/// Find a named node in a raw slice, expanding Uses nodes as needed.
fn find_node_in_raw(
    name: &str,
    raw: &[SchemaNode],
    overlay: &yangest_core::compiler::NodeOverlayMap,
    ctx: &ExpansionCtx<'_>,
) -> Option<SchemaNode> {
    for node in raw {
        match &node.kind {
            SchemaNodeKind::Uses { .. } => {
                // Expand uses and search recursively.
                let expanded = node.children(ctx);
                if let Some(found) = find_node_in_vec(name, &expanded, overlay, ctx) {
                    return Some(found);
                }
            }
            _ => {
                if node.name == name && node.is_enabled(ctx) {
                    return Some(node.clone());
                }
            }
        }
    }
    None
}

/// Find a named node in an expanded Vec, recursing into Uses.
fn find_node_in_vec(
    name: &str,
    nodes: &[SchemaNode],
    overlay: &yangest_core::compiler::NodeOverlayMap,
    ctx: &ExpansionCtx<'_>,
) -> Option<SchemaNode> {
    for node in nodes {
        match &node.kind {
            SchemaNodeKind::Uses { .. } => {
                let expanded = node.children(ctx);
                if let Some(found) = find_node_in_vec(name, &expanded, overlay, ctx) {
                    return Some(found);
                }
            }
            _ => {
                if node.name == name && node.is_enabled(ctx) {
                    return Some(node.clone());
                }
            }
        }
    }
    None
}

/// The caller fills `state.cs_records[idx]` after walking children.
fn reserve_slot(state: &mut WalkState) -> usize {
    let idx = state.cs_records.len();
    state.cs_records.push(nil()); // placeholder
    idx
}

/// Extract (ns, name) string slices from an `exs.type` Term like `{Ns, Name}`.
/// Returns static fallback strings on error.
fn exs_type_ns_name(exs_type: &Term) -> (&str, &str) {
    if let Term::Tuple(t) = exs_type {
        if t.elements.len() == 2 {
            let ns = match &t.elements[0] {
                Term::Atom(a) => a.name.as_str(),
                _ => "",
            };
            let nm = match &t.elements[1] {
                Term::Atom(a) => a.name.as_str(),
                _ => "",
            };
            return (ns, nm);
        }
    }
    ("", "")
}

/// Collect bit positions and names from a `bits` type statement's sub-statements.
/// Returns `(position, name)` pairs in declaration order, with auto-assigned positions
/// for bits without an explicit `position` sub-statement.
fn collect_bits_fields(type_stmt: &yangest_core::ast::Stmt) -> Vec<(u32, String)> {
    use yangest_core::ast::{BuiltInKeyword, Keyword};
    let mut fields = Vec::new();
    let mut next_pos: u32 = 0;
    for sub in &type_stmt.substmts {
        if matches!(&sub.keyword, Keyword::BuiltIn(BuiltInKeyword::Bit)) {
            let bit_name = sub.arg.clone().unwrap_or_default();
            let pos = sub
                .substmts
                .iter()
                .find(|s| matches!(&s.keyword, Keyword::BuiltIn(BuiltInKeyword::Position)))
                .and_then(|s| s.arg.as_deref())
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(next_pos);
            next_pos = pos + 1;
            fields.push((pos, bit_name));
        }
    }
    fields
}

/// Detect inline constraints (length, pattern, range, enumeration) in a leaf's type_stmt and
/// generate a t<hash> restriction type if constraints are present.
///
/// Returns the restriction type reference `{module_ns, t<N>}`, or the original
/// `base_exs_type` if no inline constraints are found.
/// Collect inline enum facets from a `type enumeration { ... }` statement.
/// Returns `(name_bytes, code_name_bytes, value)` in **reverse** YANG order
/// (to match yanger's foldl prepend behaviour). Returns empty vec if no enums.
fn collect_enum_facets(
    type_stmt: &yangest_core::ast::Stmt,
) -> Vec<(Vec<u8>, Option<Vec<u8>>, i64)> {
    use yangest_core::ast::{BuiltInKeyword, Keyword};
    let mut enum_vals: Vec<(Vec<u8>, Option<Vec<u8>>, i64)> = Vec::new();
    let mut next_val: i64 = 0;
    for sub in &type_stmt.substmts {
        if matches!(&sub.keyword, Keyword::BuiltIn(BuiltInKeyword::EnumStmt)) {
            let enum_name = sub.arg.clone().unwrap_or_default();
            let val = sub
                .get_substmt(BuiltInKeyword::Value)
                .and_then(|v| v.arg.as_deref())
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(next_val);
            next_val = val + 1;
            let has_snmp_name = sub.substmts.iter().any(|s| match &s.keyword {
                Keyword::Extension { module, name } => {
                    module == "tailf-common" && name == "snmp-name"
                }
                Keyword::ExtensionPrefixed { prefix: _, name } => name == "snmp-name",
                _ => false,
            });
            let code_name_bytes: Option<Vec<u8>> = if has_snmp_name {
                None
            } else {
                sub.substmts
                    .iter()
                    .find(|s| match &s.keyword {
                        Keyword::Extension { module, name } => {
                            module == "tailf-common" && name == "code-name"
                        }
                        Keyword::ExtensionPrefixed { prefix: _, name } => name == "code-name",
                        _ => false,
                    })
                    .and_then(|s| s.arg.as_deref())
                    .map(|cn| cn.as_bytes().to_vec())
            };
            enum_vals.push((enum_name.into_bytes(), code_name_bytes, val));
        }
    }
    enum_vals.reverse();
    enum_vals
}

/// Extract tailf:info or tailf:info-html text from a statement's substmts.
/// Returns (desc_bytes, flags) where flags=0 for info, 1 for info-html.
fn get_info_ext(substmts: &[yangest_core::ast::Stmt]) -> Option<(Vec<u8>, u32)> {
    use yangest_core::ast::Keyword;
    fn is_tailf_ext(kw: &Keyword, ext_name: &str) -> bool {
        match kw {
            Keyword::Extension { module, name } => module == "tailf-common" && name == ext_name,
            Keyword::ExtensionPrefixed { name, .. } => name == ext_name,
            _ => false,
        }
    }
    for sub in substmts {
        if is_tailf_ext(&sub.keyword, "info") {
            if let Some(text) = sub.arg.as_deref() {
                return Some((text.as_bytes().to_vec(), 0));
            }
        }
    }
    for sub in substmts {
        if is_tailf_ext(&sub.keyword, "info-html") {
            if let Some(text) = sub.arg.as_deref() {
                return Some((text.as_bytes().to_vec(), 1)); // F_DOC_IS_HTML = 1
            }
        }
    }
    None
}

/// Collect Misc entries for an inline enum type, mirroring Erlang's foldl-based Misc
/// accumulation in `mk_derivation` for enumeration_type_spec.
///
/// Erlang uses `foldl` over enum values, prepending each doc entry to Misc.
/// Result: REVERSE YANG order (last enum with doc ends up first in list).
fn collect_enum_misc(type_stmt: &yangest_core::ast::Stmt) -> Vec<MiscEntry> {
    use yangest_core::ast::{BuiltInKeyword, Keyword};
    let mut misc: Vec<MiscEntry> = Vec::new();
    for sub in &type_stmt.substmts {
        if matches!(&sub.keyword, Keyword::BuiltIn(BuiltInKeyword::EnumStmt)) {
            let enum_name = sub.arg.clone().unwrap_or_default().into_bytes();
            if let Some((desc, flags)) = get_info_ext(&sub.substmts) {
                misc.push(MiscEntry::EnumDoc { name: enum_name, desc, flags });
            }
        }
    }
    // foldl prepend semantics: last enum with doc goes at front of final list
    misc.reverse();
    misc
}

/// Collect Misc entries for an inline bits type, mirroring Erlang's foldr-based Misc
/// accumulation in `mk_derivation` for bits_type_spec.
///
/// Erlang uses `foldr` over bit fields with prepend, resulting in YANG forward order in Misc.
/// Within each bit's entries: code_name comes BEFORE doc (code_name is prepended after doc).
fn collect_bits_misc(type_stmt: &yangest_core::ast::Stmt) -> Vec<MiscEntry> {
    use yangest_core::ast::{BuiltInKeyword, Keyword};
    fn is_tailf_code_name(kw: &Keyword) -> bool {
        match kw {
            Keyword::Extension { module, name } => module == "tailf-common" && name == "code-name",
            Keyword::ExtensionPrefixed { name, .. } => name == "code-name",
            _ => false,
        }
    }
    let mut misc: Vec<MiscEntry> = Vec::new();
    for sub in &type_stmt.substmts {
        if matches!(&sub.keyword, Keyword::BuiltIn(BuiltInKeyword::Bit)) {
            let bit_name = sub.arg.clone().unwrap_or_default().into_bytes();
            let doc = get_info_ext(&sub.substmts);
            // Check for tailf:code-name
            let code_name_bytes: Option<Vec<u8>> = sub.substmts.iter()
                .find(|s| is_tailf_code_name(&s.keyword))
                .and_then(|s| s.arg.as_deref())
                .map(|cn| cn.as_bytes().to_vec());
            // Within a bit's Misc entries: doc is prepended first, then code_name → code_name before doc
            let has_doc = doc.is_some();
            if let Some((desc, flags)) = doc {
                misc.push(MiscEntry::BitDoc { name: bit_name.clone(), desc, flags });
            }
            if let Some(cn) = code_name_bytes {
                // code_name entry goes BEFORE the doc entry for this bit (prepended after doc)
                let insert_pos = misc.len() - if has_doc { 1 } else { 0 };
                misc.insert(insert_pos, MiscEntry::BitCodeName { name: bit_name, code_name: cn });
            }
        }
    }
    misc
}

/// variable in `collect_all_exs_types`.  This is the flag value included in the load_type
/// record that becomes part of the t<hash> computation for list/unique_list types.
///
/// Mirrors yanger_fxs's:
///   AllLoadTypeFlags = LoadTypeFlags bor (LoadDefaultFlag band F_LOAD_FXS_GET_DEFAULT)
/// where LoadTypeFlags = LoadGetFlags bor LoadSuppressEchoFlag bor LoadIsFlags
///                       bor identity_derivation_flag(UserDefinedType, Type)
///
/// For leaf-lists referencing a typedef with no added restrictions:
///   - IS_IDENTITY_DERIVATION (65536) is set because TypeSpec == BaseTypeSpec
///   - IS_ENUMERATION (2), IS_BITS (64), IS_UNION (4), or IS_EMPTY (32) are set from type
fn compute_leaf_list_all_load_type_flags(
    type_arg: &str,
    type_stmt: &yangest_core::ast::Stmt,
    tinfo: &crate::types::TypeInfo,
    module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
) -> u32 {
    use yangest_core::ast::{BuiltInKeyword, Keyword};
    use crate::types::F_EXS_IS_ENUMERATION;

    const F_LOAD_FXS_IS_ENUMERATION: u32 = 2;
    const F_LOAD_FXS_IS_BITS_LL: u32 = 64;

    if !tinfo.is_typedef {
        return 0;
    }

    // Identity derivation: set when the type statement has no added restrictions
    // (mirrors yanger_fxs's identity_derivation_flag: TypeSpec == BaseTypeSpec).
    let has_facets = type_stmt.substmts.iter().any(|s| {
        matches!(
            &s.keyword,
            Keyword::BuiltIn(
                BuiltInKeyword::Range
                    | BuiltInKeyword::Pattern
                    | BuiltInKeyword::Length
                    | BuiltInKeyword::FractionDigits
                    | BuiltInKeyword::EnumStmt
                    | BuiltInKeyword::Bit
            )
        )
    });
    let identity_derivation = if !has_facets {
        F_LOAD_FXS_IS_IDENTITY_DERIVATION
    } else {
        0
    };

    // IS_ENUMERATION: from get_type_info when TypeSpec is enumeration_type_spec
    if tinfo.extra_exs_flags & F_EXS_IS_ENUMERATION != 0 {
        return F_LOAD_FXS_IS_ENUMERATION | identity_derivation;
    }
    // IS_BITS: from get_type_info when TypeSpec is bits_type_spec
    if crate::types::is_bits_base(type_arg, module_name, registry) {
        return F_LOAD_FXS_IS_BITS_LL | identity_derivation;
    }
    let base_is_union = is_union_base(type_arg, module_name, registry);
    let base_is_empty = !base_is_union && is_empty_base(type_arg, module_name, registry);
    if let Some(ref td_mod) = tinfo.typedef_defining_module {
        let is_builtin_mod = matches!(
            td_mod.as_str(),
            "ietf-inet-types" | "ietf-yang-types" | "tailf-common" | "tailf-inet-types"
        );
        let mut flags = identity_derivation;
        // IS_UNION/IS_EMPTY are additive (from LoadIsFlags in get_type_info)
        if base_is_union && is_builtin_mod {
            flags |= F_LOAD_FXS_IS_UNION;
        } else if base_is_empty {
            flags |= F_LOAD_FXS_IS_EMPTY;
        }
        flags
    } else {
        0
    }
}

/// Check whether a union type has any cross-module typedef members.
/// A member is cross-module if it comes from a different module than `node_module_name`
/// AND is not from a builtin/standard module.
fn union_has_cross_module_member(
    union_type_stmt: &yangest_core::ast::Stmt,
    node_module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
) -> bool {
    use yangest_core::ast::{BuiltInKeyword, Keyword};
    for sub in &union_type_stmt.substmts {
        if !matches!(&sub.keyword, Keyword::BuiltIn(BuiltInKeyword::Type)) {
            continue;
        }
        let member_arg = sub.arg.as_deref().unwrap_or("string");
        let member_info =
            crate::types::type_info_with_registry(member_arg, node_module_name, registry);
        let is_local_or_builtin = member_info
            .typedef_defining_module
            .as_ref()
            .map_or(true, |td_mod| {
                td_mod.as_str() == node_module_name
                    || matches!(
                        td_mod.as_str(),
                        "ietf-inet-types"
                            | "ietf-yang-types"
                            | "tailf-common"
                            | "tailf-inet-types"
                    )
            });
        if !is_local_or_builtin {
            return true;
        }
    }
    false
}

/// Collect type refs for inline union member types.
/// `member_load_flags`: the LoadTypeFlags to use for member type hash/load_type records.
/// Matches the union's own load_type_flags (IS_UNION=4 or GET_TYPE_INFO|... for cross-module).
fn collect_inline_union_members(
    union_type_stmt: &yangest_core::ast::Stmt,
    node_module_name: &str,
    module_ns: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
    module_ns_cache: &std::collections::HashMap<String, String>,
    type_gen: &mut TypeGen,
    member_load_flags: u32,
) -> (Vec<Term>, Vec<Term>) {
    use yangest_core::ast::{BuiltInKeyword, Keyword};

    let mut member_refs: Vec<Term> = Vec::new();
    let mut member_primitives: Vec<Term> = Vec::new();

    for sub in &union_type_stmt.substmts {
        if !matches!(&sub.keyword, Keyword::BuiltIn(BuiltInKeyword::Type)) {
            continue;
        }
        let member_arg = sub.arg.as_deref().unwrap_or("string");

        let member_info =
            crate::types::type_info_with_registry(member_arg, node_module_name, registry);

        // Always use the typedef's {namespace, name} reference directly.
        // yanger keeps union members as typedef refs (not flattened/resolved to XSD base).
        let final_ref = maybe_generate_leaf_type(
            type_gen,
            module_ns,
            member_arg,
            member_info.exs_type.clone(),
            sub,
            node_module_name,
            registry,
            module_ns_cache,
            member_load_flags,
            false, // union members are never "mandatory or key" for load_type_flags purposes
        )
        .0;

        member_refs.push(final_ref);
        member_primitives.push(member_info.primitive_type.clone());
    }

    (member_refs, member_primitives)
}

fn find_union_typedef(
    type_arg: &str,
    node_module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
) -> Option<(String, yangest_core::ast::Stmt)> {
    let (td_module_name, local_name) =
        resolve_type_arg_to_module(type_arg, node_module_name, registry)?;
    let td_module = registry.resolve_import(&td_module_name, None)?;
    let typedef = td_module.typedefs.get(&local_name)?;
    if typedef.type_stmt.arg.as_deref() == Some("union") {
        Some((td_module_name, typedef.type_stmt.clone()))
    } else {
        None
    }
}

fn resolve_type_arg_to_module(
    type_arg: &str,
    node_module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
) -> Option<(String, String)> {
    let (prefix, local) = if let Some(pos) = type_arg.find(':') {
        (&type_arg[..pos], type_arg[pos + 1..].to_string())
    } else {
        ("", type_arg.to_string())
    };

    let node_module = registry.resolve_import(node_module_name, None)?;
    let module_name = if prefix.is_empty() {
        if node_module.typedefs.contains_key(&local) {
            node_module_name.to_string()
        } else {
            return None;
        }
    } else if prefix == node_module.prefix {
        node_module_name.to_string()
    } else {
        node_module.prefix_map.get(prefix)?.clone()
    };

    Some((module_name, local))
}

fn resolve_to_xsd_base(
    type_arg: &str,
    node_module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
) -> Term {
    let Some((mut current_module, mut current_local)) =
        resolve_type_arg_to_module(type_arg, node_module_name, registry)
    else {
        return tuple(vec![
            atom("http://www.w3.org/2001/XMLSchema"),
            atom("string"),
        ]);
    };

    let xsd_ns = "http://www.w3.org/2001/XMLSchema";

    for _ in 0..20 {
        let Some(td_mod) = registry.resolve_import(&current_module, None) else {
            break;
        };
        let Some(typedef) = td_mod.typedefs.get(&current_local) else {
            break;
        };

        let base_arg = typedef.type_stmt.arg.as_deref().unwrap_or("string");
        if is_yang_builtin(base_arg) {
            let xsd_name = match base_arg {
                "string" => "string",
                "boolean" => "boolean",
                "int8" => "byte",
                "int16" => "short",
                "int32" => "int",
                "int64" => "long",
                "uint8" => "unsignedByte",
                "uint16" => "unsignedShort",
                "uint32" => "unsignedInt",
                "uint64" => "unsignedLong",
                "decimal64" => "decimal",
                "binary" => "hexBinary",
                "empty" => "string",
                _ => "string",
            };
            return tuple(vec![atom(xsd_ns), atom(xsd_name)]);
        }

        if base_arg.contains(':') {
            let Some((next_module, next_local)) =
                resolve_type_arg_to_module(base_arg, &td_mod.key.name, registry)
            else {
                break;
            };
            let is_builtin_mod = matches!(
                next_module.as_str(),
                "ietf-inet-types" | "ietf-yang-types" | "tailf-common" | "tailf-inet-types"
            );
            if is_builtin_mod {
                if let Some(target_mod) = registry.resolve_import(&next_module, None) {
                    return tuple(vec![atom(&target_mod.namespace), atom(&next_local)]);
                }
                break;
            }
            current_module = next_module;
            current_local = next_local;
        } else if td_mod.typedefs.contains_key(base_arg) {
            current_local = base_arg.to_string();
        } else {
            break;
        }
    }

    tuple(vec![atom(xsd_ns), atom("string")])
}

fn maybe_generate_leaf_type(
    type_gen: &mut TypeGen,
    module_ns: &str,
    type_arg: &str,
    base_exs_type: Term,
    type_stmt: &yangest_core::ast::Stmt,
    node_module_name: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
    module_ns_cache: &std::collections::HashMap<String, String>,
    // LoadTypeFlags for enum/bits: 4=IS_UNION for union members, 0=type-specific default (enum=2, bits=64).
    // For union itself, this is computed based on member types (cross-module vs builtin).
    load_flags: u32,
    // Whether the containing leaf is mandatory or a list key.
    // Affects whether GET_DEFAULT is included in union load_type_flags for cross-module unions.
    is_mandatory_or_key: bool,
) -> (Term, Option<Term>) {
    use yangest_core::ast::{BuiltInKeyword, Keyword};

    // Identityref: create a restriction type with ignore_facet for each base identity.
    // Mirrors yanger_fxs mk_derivation for identityref_type_spec.
    if type_arg == "identityref" {
        let bases: Vec<(String, String)> = type_stmt
            .substmts
            .iter()
            .filter(|s| matches!(&s.keyword, Keyword::BuiltIn(BuiltInKeyword::Base)))
            .filter_map(|s| s.arg.as_deref())
            .filter_map(|base_ref| resolve_identity_base_to_ns_name(base_ref, node_module_name, registry, module_ns_cache))
            .collect();
        if !bases.is_empty() {
            use crate::thash::{encode_ignore_facet_bytes, ignore_facet_eetf};
            let facet_bytes: Vec<Vec<u8>> = bases.iter()
                .map(|(ns, name)| encode_ignore_facet_bytes(ns, name))
                .collect();
            let facets_eetf: Vec<Term> = bases.iter()
                .map(|(ns, name)| ignore_facet_eetf(ns, name))
                .collect();
            // identityref restriction type: flags=2 (F_EXS_TYPE_IS_GENERATED_BY_YANGER)
            // Include load_type in hash when in union member context (load_flags != 0)
            let type_ref = type_gen.get_or_create_restriction_type_with_load_flags(
                module_ns,
                "http://tail-f.com/ns/confd/1.0",
                "identityref",
                facet_bytes,
                facets_eetf,
                2,
                load_flags,
            );
            return (type_ref, None);
        }
        return (base_exs_type, None);
    }


    if type_arg == "union" {
        // Determine cross-module status first (before collecting refs) so we
        // know the correct member_load_flags = union_load_type_flags upfront.
        // yanger passes the union's own LoadTypeFlags to each member via mk_union().
        let any_cross_module =
            union_has_cross_module_member(type_stmt, node_module_name, registry);
        // Compute load_type_flags for the union's own hash/load_type record:
        // - cross-module typedef members: GET_TYPE_INFO (+ GET_DEFAULT if non-mandatory/non-key)
        // - all-builtin or same-module members: IS_UNION = 4
        let union_load_type_flags = if any_cross_module {
            let mut f = F_LOAD_FXS_GET_TYPE_INFO;
            if !is_mandatory_or_key {
                f |= F_LOAD_FXS_GET_DEFAULT;
            }
            f
        } else {
            F_LOAD_FXS_IS_UNION
        };
        // Collect member refs, passing union_load_type_flags as the member_load_flags so
        // each member's load_type.flags matches the union's own flags (matching yanger's
        // mk_union() which passes LoadTypeFlags unchanged to each member's mk_exs_dot_type).
        let (member_refs, member_primitives) = collect_inline_union_members(
            type_stmt,
            node_module_name,
            module_ns,
            registry,
            module_ns_cache,
            type_gen,
            union_load_type_flags,
        );
        if member_refs.is_empty() {
            return (base_exs_type, None);
        }
        let type_ref =
            type_gen.get_or_create_union_type(module_ns, member_refs, union_load_type_flags);
        // primitive_type for a union exs node is the list of member primitive types.
        let prim = if member_primitives.is_empty() {
            None
        } else {
            Some(list(member_primitives))
        };
        return (type_ref, prim);
    }

    // Inline enumeration: generate t<hash> for the enumeration itself
    if type_arg == "enumeration" {
        let enum_vals = collect_enum_facets(type_stmt);
        if enum_vals.is_empty() {
            return (base_exs_type, None);
        }
        let flags = if load_flags == 0 { 2 } else { load_flags }; // 0 = use IS_ENUMERATION=2
        let misc = collect_enum_misc(type_stmt);
        return (
            type_gen.get_or_create_enum_type(module_ns, &enum_vals, flags, &misc),
            None,
        );
    }

    // Inline bits type: collect bit sub-statements and generate t<hash> type
    if type_arg == "bits" {
        let fields = collect_bits_fields(type_stmt);
        if !fields.is_empty() {
            let max_pos = fields.iter().map(|(p, _)| *p).max().unwrap_or(0);
            let size = bits_type_size(max_pos);
            let flags = if load_flags == 0 { 64 } else { load_flags }; // 0 = use IS_BITS=64
            let misc = collect_bits_misc(type_stmt);
            return (
                type_gen.get_or_create_bits_type(module_ns, fields, size, flags, &misc),
                None,
            );
        }
        return (base_exs_type, None);
    }

    // Check if there are any inline constraints
    let has_length = type_stmt
        .substmts
        .iter()
        .any(|s| matches!(&s.keyword, Keyword::BuiltIn(BuiltInKeyword::Length)));
    let has_pattern = type_stmt
        .substmts
        .iter()
        .any(|s| matches!(&s.keyword, Keyword::BuiltIn(BuiltInKeyword::Pattern)));
    let has_range = type_stmt
        .substmts
        .iter()
        .any(|s| matches!(&s.keyword, Keyword::BuiltIn(BuiltInKeyword::Range)));
    let has_fraction_digits = type_stmt
        .substmts
        .iter()
        .any(|s| matches!(&s.keyword, Keyword::BuiltIn(BuiltInKeyword::FractionDigits)));

    if !has_length && !has_pattern && !has_range && !has_fraction_digits {
        return (base_exs_type, None);
    }

    let (base_ns, base_name) = exs_type_ns_name(&base_exs_type);
    if base_ns.is_empty() {
        return (base_exs_type, None);
    }

    let xsd_tag = yang_int_to_xsd_info(type_arg)
        .map(|(_, tag)| tag)
        .unwrap_or(8);

    let mut facet_bytes: Vec<Vec<u8>> = Vec::new();
    let mut facets_eetf: Vec<Term> = Vec::new();
    let mut flags: u32 = 2;

    for s in &type_stmt.substmts {
        if matches!(&s.keyword, Keyword::BuiltIn(BuiltInKeyword::Length)) {
            if let Some(arg) = &s.arg {
                let ranges = parse_length_ranges(arg);
                facet_bytes.push(encode_length_facet_bytes(&ranges));
                facets_eetf.push(length_facet_eetf(&ranges));
            }
        }
    }

    for s in &type_stmt.substmts {
        if matches!(&s.keyword, Keyword::BuiltIn(BuiltInKeyword::Pattern)) {
            if let Some(arg) = &s.arg {
                flags |= 4;
                let pat = arg.as_bytes().to_vec();
                facet_bytes.push(encode_pattern_facet_bytes(&pat));
                facets_eetf.push(pattern_facet_eetf(&pat));
            }
        }
    }

    for s in &type_stmt.substmts {
        if matches!(&s.keyword, Keyword::BuiltIn(BuiltInKeyword::Range)) {
            if let Some(arg) = &s.arg {
                let ranges = parse_range_bounds(arg, xsd_tag);
                facet_bytes.push(encode_range_facet_bytes(&ranges));
                facets_eetf.push(range_facet_eetf(&ranges));
            }
        }
    }

    for s in &type_stmt.substmts {
        if matches!(&s.keyword, Keyword::BuiltIn(BuiltInKeyword::FractionDigits)) {
            if let Some(arg) = &s.arg {
                if let Ok(n) = arg.trim().parse::<u8>() {
                    facet_bytes.push(encode_fraction_digits_facet_bytes(n));
                    facets_eetf.push(fraction_digits_facet_eetf(n));
                }
            }
        }
    }

    if facet_bytes.is_empty() {
        return (base_exs_type, None);
    }

    (
        type_gen.get_or_create_restriction_type_with_load_flags(
            module_ns,
            base_ns,
            base_name,
            facet_bytes,
            facets_eetf,
            flags,
            load_flags, // include load_type in hash when in union member context
        ),
        None,
    )
}

fn make_cs_node(
    ns: &str,
    ns_hash: u128,
    htag: Term,
    exs: Term,
    keys: Term,
    flags: u128,
    dbm: Term,
    dba: Term,
    cmp: Term,
    extra: Term,
) -> Term {
    make_cs_node_with_hidden(
        ns,
        ns_hash,
        htag,
        exs,
        keys,
        flags,
        dbm,
        dba,
        cmp,
        extra,
        atom("none"),
        bigint(0),
    )
}

fn make_cs_node_with_hidden(
    ns: &str,
    ns_hash: u128,
    htag: Term,
    exs: Term,
    keys: Term,
    flags: u128,
    dbm: Term,
    dba: Term,
    cmp: Term,
    extra: Term,
    hidden: Term,
    cli_flags: Term,
) -> Term {
    make_cs(
        nil(),
        htag,
        atom(ns),
        bigint(ns_hash),
        exs,
        keys,
        bigint(flags),
        dbm,
        dba,
        nil(),
        nil(),
        cmp,
        nil(),
        hidden,
        nil(),
        undefined(),
        extra,
        undefined(),
        nil(),
        cli_flags,
        undefined(),
        nil(),
        nil(),
    )
}

/// Build a tagpath list term from an already-built `Vec<Term>` slice.
///
/// Each element is already a Term (either `atom(name)` for regular nodes or
/// `improper_list_pair(atom(ns), atom(name))` for augmented nodes).
fn tagpath_term(path: &[Term]) -> Term {
    list(path.to_vec())
}

/// Encode a leaf's default value as an internal Erlang term, mirroring
/// `yanger_fxs:mk_internal_value/4`.
///
/// When `load_flags` includes `F_LOAD_FXS_GET_DEFAULT`, the default is
/// deferred to load time; store the raw binary string (which the loader
/// will parse).  Otherwise, convert the string to a typed internal value:
///
/// - Integer types → `{TypeId, integer}`, except int32 which is bare integer
/// Look up the integer ordinal of an enum value `default_str` in typedef `type_arg`
/// (which may be prefixed like "module:name" or unqualified) from `node_module_name`.
/// Returns `Some(value)` when the enum definition is found, `None` otherwise.
fn lookup_enum_ordinal(
    type_arg: &str,
    node_module_name: &str,
    default_str: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
) -> Option<i64> {
    // Resolve the module and local typedef name.
    let (prefix_opt, local) = if let Some(pos) = type_arg.find(':') {
        (Some(&type_arg[..pos]), &type_arg[pos + 1..])
    } else {
        (None, type_arg)
    };
    let node_module = registry.resolve_import(node_module_name, None)?;
    let module_name = if let Some(prefix) = prefix_opt {
        node_module.prefix_map.get(prefix).cloned().or_else(|| {
            if node_module.prefix == prefix {
                Some(node_module.key.name.clone())
            } else {
                None
            }
        })?
    } else {
        node_module.key.name.clone()
    };
    let td_module = registry.resolve_import(&module_name, None)?;
    let typedef = td_module.typedefs.get(local)?;

    // Walk the typedef's type statement looking for enum substmts.
    // Recurse through typedef chains.
    find_enum_ordinal_in_type(&typedef.type_stmt, &module_name, default_str, registry, 8)
}

fn find_enum_ordinal_in_type(
    type_stmt: &yangest_core::ast::Stmt,
    module_name: &str,
    default_str: &str,
    registry: &yangest_core::compiler::ModuleRegistry,
    depth: usize,
) -> Option<i64> {
    use yangest_core::ast::{BuiltInKeyword, Keyword};
    if depth == 0 {
        return None;
    }
    // If this type_stmt has enum substmts, search them directly.
    let has_enums = type_stmt
        .substmts
        .iter()
        .any(|s| matches!(&s.keyword, Keyword::BuiltIn(BuiltInKeyword::EnumStmt)));
    if has_enums {
        let mut next_val: i64 = 0;
        for sub in &type_stmt.substmts {
            if matches!(&sub.keyword, Keyword::BuiltIn(BuiltInKeyword::EnumStmt)) {
                let enum_name = sub.arg.as_deref().unwrap_or_default();
                let val = sub
                    .get_substmt(BuiltInKeyword::Value)
                    .and_then(|v| v.arg.as_deref())
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(next_val);
                next_val = val + 1;
                if enum_name == default_str {
                    return Some(val);
                }
            }
        }
        return None;
    }
    // Otherwise, try to resolve the base typedef.
    let base_arg = type_stmt.arg.as_deref()?;
    // Split off prefix if any.
    let (prefix_opt, local) = if let Some(pos) = base_arg.find(':') {
        (Some(&base_arg[..pos]), &base_arg[pos + 1..])
    } else {
        (None, base_arg)
    };
    let resolved_module = if let Some(prefix) = prefix_opt {
        let base_mod = registry.resolve_import(module_name, None)?;
        let mn = base_mod.prefix_map.get(prefix).cloned().or_else(|| {
            if base_mod.prefix == prefix {
                Some(base_mod.key.name.clone())
            } else {
                None
            }
        })?;
        mn
    } else {
        module_name.to_string()
    };
    let td_module = registry.resolve_import(&resolved_module, None)?;
    let typedef = td_module.typedefs.get(local)?;
    find_enum_ordinal_in_type(
        &typedef.type_stmt,
        &resolved_module,
        default_str,
        registry,
        depth - 1,
    )
}

/// Returns true if a primitive_type can be pre-computed as an internal value by yanger's
/// `mk_internal_value`.  Types NOT in this set (e.g. `dateTime`, `inetAddressIPv4`) use a
/// `string_type_spec` base in yanger but have a non-`string` primitive_type, causing
/// `mk_internal_value` to return `undefined` — which triggers `F_LOAD_FXS_PARSE_DEFAULT`.
fn prim_type_is_precomputable(name: &str) -> bool {
    matches!(
        name,
        // Plain string types
        "string"
        // Boolean
        | "boolean"
        // Signed integers
        | "byte" | "short" | "int" | "long"
        // Unsigned integers
        | "unsignedByte" | "unsignedShort" | "unsignedInt" | "unsignedLong"
        // SNMP / counter types (unsigned)
        | "Counter32" | "Counter64" | "Gauge32" | "TimeTicks"
        // Decimal64 is pre-computable (yanger stores {BDECIMAL64, {value, frac_digits}})
        | "decimal64"
        // Bits types (pre-computable bitmask)
        | "bits_type_32" | "bits_type_64" | "bits_type_big"
        // Empty type has no default value ever
        | "empty"
    )
}

/// - boolean       → atom `true` / `false`
/// - string        → binary (the raw string)
/// - all others    → binary (fallback; loader will re-parse)
fn encode_internal_default(default_str: &str, prim_type: &Term, load_flags: u32) -> Term {
    // When GET_DEFAULT or PARSE_DEFAULT is set, the loader fetches/parses the default at startup.
    // yanger stores DefaultStr (a binary) in the exs record in this case.
    if load_flags & (F_LOAD_FXS_GET_DEFAULT | F_LOAD_FXS_PARSE_DEFAULT) != 0 {
        return binary_str(default_str);
    }

    // Map primitive_type atom name → XSD type ID (from xsd.hrl)
    // and encode accordingly.
    let prim_name = match prim_type {
        Term::Atom(a) => a.name.as_str(),
        _ => return binary_str(default_str), // undefined or complex
    };

    match prim_name {
        // Integer types — wrap in {TypeId, integer}
        "byte" => encode_int_default(default_str, 6), // BINT8
        "short" => encode_int_default(default_str, 7), // BINT16
        "int" => {
            // BINT32 — bare integer
            if let Ok(n) = default_str.trim().parse::<i64>() {
                int(n as i32)
            } else {
                binary_str(default_str)
            }
        }
        "long" => encode_int_default(default_str, 9), // BINT64
        "unsignedByte" => encode_int_default(default_str, 10), // BUINT8
        "unsignedShort" => encode_int_default(default_str, 11), // BUINT16
        "unsignedInt" => encode_int_default(default_str, 12), // BUINT32
        "unsignedLong" => encode_int_default(default_str, 13), // BUINT64
        "Counter32" | "Gauge32" | "TimeTicks" => encode_int_default(default_str, 12), // BUINT32
        "Counter64" => encode_int_default(default_str, 13), // BUINT64
        // Boolean — store atom true/false
        "boolean" => match default_str.trim() {
            "true" => atom("true"),
            "false" => atom("false"),
            _ => binary_str(default_str),
        },
        // String — binary is already the correct internal form
        "string" => binary_str(default_str),
        // All other types (decimal64, bits, union, identityref, …) fall back
        // to raw binary; the loader will parse at startup.
        _ => binary_str(default_str),
    }
}

/// Helper: parse `s` as an integer and wrap in `{type_id, integer}`.
/// Falls back to binary on parse failure.
fn encode_int_default(s: &str, type_id: i32) -> Term {
    if let Ok(n) = s.trim().parse::<i64>() {
        let val = if n >= 0 {
            bigint(n as u128)
        } else if n >= i32::MIN as i64 {
            int(n as i32)
        } else {
            // Negative values outside i32 range (e.g. large negative int64)
            use eetf::BigInteger;
            use num_bigint::BigInt;
            Term::from(BigInteger {
                value: BigInt::from(n),
            })
        };
        tuple(vec![int(type_id), val])
    } else {
        binary_str(s)
    }
}

/// Compute the sort order (cmp) value for a list or leaf-list node.
/// Mirrors `choose_sort_order_type` in yanger_fxs.erl.
///
/// CS_CMP_NORMAL=0, CS_CMP_SNMP=1, CS_CMP_SNMP_IMPLIED=2, CS_CMP_USER=3, CS_CMP_UNSORTED=4
fn sort_order_cmp(
    ordered_by: &OrderedBy,
    extensions: &[ExtensionInstance],
    mode: SubtreeMode,
) -> Term {
    // Inside input/output/notification subtrees: always ordered-by-user
    if matches!(
        mode,
        SubtreeMode::ActionInput | SubtreeMode::ActionOutput | SubtreeMode::Notification
    ) {
        return int(3); // CS_CMP_USER
    }
    if matches!(ordered_by, OrderedBy::User) {
        return int(3); // CS_CMP_USER
    }
    // Check for tailf:sort-order
    for ext in extensions {
        if ext.module == "tailf-common" && ext.name == "sort-order" {
            match ext.arg.as_deref() {
                Some("snmp") => return int(1),
                Some("snmp-implied") => return int(2),
                Some("unsorted") => return int(4),
                _ => {}
            }
        }
    }
    int(0) // CS_CMP_NORMAL
}

/// Returns the `{status, deprecated}` or `{status, obsolete}` extra term item
/// and `F_CS_YANG_STATUS` flag when the node has non-current status.
fn yang_status_items(status: &Status) -> (Vec<Term>, u128) {
    match status {
        Status::Deprecated => (
            vec![tuple(vec![atom("status"), atom("deprecated")])],
            F_CS_YANG_STATUS,
        ),
        Status::Obsolete => (
            vec![tuple(vec![atom("status"), atom("obsolete")])],
            F_CS_YANG_STATUS,
        ),
        Status::Current => (vec![], 0),
    }
}

/// Merge case_depth_extra into an existing extra list, inserting case_depth after load_flags
/// (if present) or at the front (if load_flags is absent).
///
/// Reference (confdc) ordering: `[load_flags?, case_depth, units?, ...]`
/// - When load_flags is present: `[load_flags, case_depth]`
/// - When units is present (no load_flags): `[case_depth, units]`
fn merge_case_depth(extra_term: Term, case_depth_extra: Vec<Term>) -> Term {
    if case_depth_extra.is_empty() {
        return extra_term;
    }
    let existing: Vec<Term> = match extra_term {
        Term::List(ref x) => x.elements.clone(),
        _ => vec![],
    };
    // Find the position of load_flags (if any) — insert case_depth after it.
    let insert_pos = existing
        .iter()
        .position(|t| {
            if let Term::Tuple(tup) = t {
                tup.elements
                    .first()
                    .map(|e| matches!(e, Term::Atom(a) if a.name == "load_flags"))
                    .unwrap_or(false)
            } else {
                false
            }
        })
        .map(|i| i + 1)
        .unwrap_or(0); // after load_flags, or at front if absent
    let mut result = existing[..insert_pos].to_vec();
    result.extend(case_depth_extra);
    result.extend_from_slice(&existing[insert_pos..]);
    list(result)
}

/// Apply when/must XPath expressions to a compiled CS term.
///
/// The CS tuple layout (0-indexed):
///   0=cs, 1=tagpath, 2=htag, 3=ns, 4=hns, 5=exs, 6=keys, 7=flags,
///   8=dbm, 9=dba, 10=validatemfas, 11=actions, 12=cmp, 13=hooks,
///   14=hidden, 15=notifs, 16=symlink, 17=extra, ...
///
/// Prepends `{'when',[...]}` to `extra`, merges `load_flags`, and sets `validatemfas`.
fn apply_when_must_to_cs(
    mut cs: Term,
    node: &SchemaNode,
    parent: Option<&SchemaNode>,
    module: &CompiledModule,
    ctx: &ExpansionCtx<'_>,
) -> Term {
    // Compute when extra items.
    let (when_items, when_load_flags) = build_when_extra(node, module, ctx);
    // Compute must vmfas.
    let (vmfas, must_load_flags, _cs_flags) = build_must_vmfas(node, parent, module, ctx);

    if when_items.is_empty() && vmfas.is_empty() {
        return cs;
    }

    let combined_load_flags = when_load_flags | must_load_flags;

    if let Term::Tuple(ref mut tup) = cs {
        // Update validatemfas (index 10).
        if !vmfas.is_empty() {
            tup.elements[10] = list(vmfas);
        }

        // Update extra (index 17): prepend when items and merge load_flags.
        if !when_items.is_empty() || combined_load_flags != 0 {
            tup.elements[17] =
                merge_when_into_extra(tup.elements[17].clone(), when_items, combined_load_flags);
        }
    }
    cs
}

/// Merge when extra items and load_flags into an existing extra term.
///
/// Ordering mirrors yanger_fxs two-step process:
/// - `add_cs0` appends `{load_flags, N}` LAST for when-only load flags.
/// - `set_type` (CsExtra3) extracts any existing `load_flags`, OR-merges with
///   type load flags, and PREPENDS the combined result.
///
/// So the rule is:
/// - If there is an existing `{load_flags, N}` from type processing: merge and put FIRST.
/// - If there is only a newly added when-load_flags with no pre-existing: put LAST.
fn merge_when_into_extra(extra_term: Term, when_items: Vec<Term>, extra_load_flags: u32) -> Term {
    if when_items.is_empty() && extra_load_flags == 0 {
        return extra_term;
    }

    let existing: Vec<Term> = match &extra_term {
        Term::List(x) => x.elements.clone(),
        _ => vec![],
    };

    // Separate existing load_flags entry from other items.
    let mut existing_load_flags: u32 = 0;
    let mut had_existing_load_flags = false;
    let mut other_items: Vec<Term> = Vec::with_capacity(existing.len());
    for item in existing {
        let is_load_flags = if let Term::Tuple(ref t) = item {
            t.elements
                .first()
                .map(|e| matches!(e, Term::Atom(a) if a.name == "load_flags"))
                .unwrap_or(false)
        } else {
            false
        };
        if is_load_flags {
            had_existing_load_flags = true;
            if let Term::Tuple(ref t) = item {
                existing_load_flags |= match t.elements.get(1) {
                    Some(Term::BigInteger(bi)) => {
                        let (_, bytes) = bi.value.to_bytes_le();
                        let mut buf = [0u8; 4];
                        let len = bytes.len().min(4);
                        buf[..len].copy_from_slice(&bytes[..len]);
                        u32::from_le_bytes(buf)
                    }
                    Some(Term::FixInteger(fi)) => fi.value as u32,
                    _ => 0,
                };
            }
        } else {
            other_items.push(item);
        }
    }

    let combined_load_flags = existing_load_flags | extra_load_flags;
    let mut result = Vec::with_capacity(other_items.len() + when_items.len() + 1);

    if had_existing_load_flags && combined_load_flags != 0 {
        // Existing type load_flags: merged result goes FIRST (yanger set_type CsExtra3 prepend).
        result.push(tuple(vec![
            atom("load_flags"),
            crate::terms::uint(combined_load_flags),
        ]));
        result.extend(other_items);
        result.extend(when_items);
    } else {
        // Only when-derived load_flags: goes LAST (yanger add_cs0 append pattern).
        result.extend(other_items);
        result.extend(when_items);
        if combined_load_flags != 0 {
            result.push(tuple(vec![
                atom("load_flags"),
                crate::terms::uint(combined_load_flags),
            ]));
        }
    }
    list(result)
}
/// `meta_prefix` items (from tailf:meta-data extensions) are prepended first.
/// `suffix` items (e.g., cli HasSubstatements extra records) are appended last.
fn node_extra_with_meta(
    meta_prefix: Vec<Term>,
    status: &Status,
    units: Option<&str>,
    load_flags: u32,
) -> (Term, u128) {
    node_extra_with_meta_suffix(meta_prefix, vec![], status, units, load_flags)
}

fn node_extra_with_meta_suffix(
    meta_prefix: Vec<Term>,
    suffix: Vec<Term>,
    status: &Status,
    units: Option<&str>,
    load_flags: u32,
) -> (Term, u128) {
    let (status_items, status_flag) = yang_status_items(status);
    let mut items = meta_prefix;
    items.extend(status_items);
    if let Some(u) = units {
        items.push(tuple(vec![atom("units"), binary_str(u)]));
    }
    if load_flags != 0 {
        items.push(tuple(vec![atom("load_flags"), uint(load_flags)]));
    }
    items.extend(suffix);
    let term = if items.is_empty() { nil() } else { list(items) };
    (term, status_flag)
}

fn node_extra(status: &Status, units: Option<&str>, load_flags: u32) -> (Term, u128) {
    node_extra_with_meta(vec![], status, units, load_flags)
}

/// Extract tailf:hidden value, tailf:meta-data entries, and tailf:alt-name
/// Encode tailf:hidden extension values as an Erlang term, mirroring yanger_fxs get_hidden/2.
///
/// Returns:
/// - `atom("none")` if no hidden values
/// - `atom("full")` if the only value is "full"
/// - `list([atom("obsolete"), ...])` for any other combination
///
/// Note: OldHidden (parent) is passed in so we can return it when there are no local values.
fn get_hidden(extensions: &[ExtensionInstance], old_hidden: &Term) -> Term {
    // Collect unique hidden values (dedup: schema compiler may duplicate annotation extensions).
    let mut seen = std::collections::BTreeSet::new();
    let values: Vec<&str> = extensions
        .iter()
        .filter(|e| e.module == "tailf-common" && e.name == "hidden")
        .filter_map(|e| e.arg.as_deref())
        .filter(|v| seen.insert(*v))
        .collect();
    match values.as_slice() {
        [] => old_hidden.clone(),
        ["full"] => atom("full"),
        _ => list(values.iter().map(|v| atom(v)).collect()),
    }
}


///
/// Returns `(hidden_term, simple_extra_items, extra_flags)` where:
/// - `hidden_term`: result of `get_hidden(extensions, &atom("none"))`
/// - `simple_extra_items`: extra records from alt-name, meta-data, etc.
/// - `extra_flags`: cs flags OR-ed in (F_CS_META_DATA, F_CS_CLI_NAME)
///
/// Note: F_CS_CHILD_READ_ONLY is NOT set here — it is set solely based on READ/WRITE flags
/// (oper nodes) in the caller, mirroring yanger_fxs `child_flags/3`.
fn tailf_hidden_and_meta(extensions: &[ExtensionInstance]) -> (Term, Vec<Term>, u128) {
    let mut meta_kv: Vec<Term> = Vec::new();
    let mut extra_items: Vec<Term> = Vec::new();
    let mut extra_flags: u128 = 0;

    for ext in extensions {
        if ext.module != "tailf-common" {
            continue;
        }
        match ext.name.as_str() {
            "hidden" => {} // handled by get_hidden below
            "meta-data" => {
                if let Some(ref key) = ext.arg {
                    // Find tailf:meta-value sub-statement (may still be in ExtensionPrefixed form
                    // since substmts of ExtensionInstance are stored as raw parsed Stmts).
                    let value = ext
                        .substmts
                        .iter()
                        .find(|s| match &s.keyword {
                            Keyword::Extension { name: n, .. } => n == "meta-value",
                            Keyword::ExtensionPrefixed { name: n, .. } => n == "meta-value",
                            _ => false,
                        })
                        .and_then(|s| s.arg.clone())
                        .unwrap_or_default();
                    meta_kv.push(tuple(vec![
                        Term::from(eetf::Binary {
                            bytes: key.as_bytes().to_vec(),
                        }),
                        Term::from(eetf::Binary {
                            bytes: value.as_bytes().to_vec(),
                        }),
                    ]));
                }
            }
            "alt-name" => {
                if let Some(ref name_val) = ext.arg {
                    extra_items.push(tuple(vec![
                        atom("cli_name"),
                        Term::from(eetf::Binary {
                            bytes: name_val.as_bytes().to_vec(),
                        }),
                    ]));
                    extra_flags |= F_CS_CLI_NAME;
                }
            }
            _ => {}
        }
    }

    let hidden_term = get_hidden(extensions, &atom("none"));

    if !meta_kv.is_empty() {
        extra_flags |= F_CS_META_DATA;
        extra_items.push(tuple(vec![atom("meta_data"), list(meta_kv)]));
    }

    (hidden_term, extra_items, extra_flags)
}

/// Compute F_CS_DOC_DESCRIPTION flag: set when node has description or tailf:info text.
/// Mirrors yanger_fxs `mk_doc` which sets F_CS_DOC_DESCRIPTION when Doc#doc.desc != undefined.
fn doc_description_flag(extensions: &[ExtensionInstance], _description: &Option<String>) -> u128 {
    // F_CS_DOC_DESCRIPTION is set only when tailf:info or tailf:info-html is present.
    // Plain YANG description does NOT trigger it (yanger mk_doc2 only falls back to description
    // when Ctx.use_description != false, which is not the case for confdc/IOS-XE compilation).
    let has_tailf_info = extensions.iter().any(|e| {
        e.module == "tailf-common" && (e.name == "info" || e.name == "info-html") && e.arg.is_some()
    });
    if has_tailf_info {
        F_CS_DOC_DESCRIPTION
    } else {
        0
    }
}

/// Build a `{doc, {doc, Tagpath}, DescBin, Flags, undefined}` record if the node
/// has a `tailf:info` or `tailf:info-html` extension.  Returns `None` if absent.
/// Mirrors yanger_fxs `mk_doc`/`mk_doc2` with `Force=false` and `use_description=false`.
fn make_node_doc_term(extensions: &[ExtensionInstance], tagpath: &[Term]) -> Option<Term> {
    // F_DOC_IS_HTML = 1 for tailf:info-html
    const F_DOC_IS_HTML: u32 = 1;
    for e in extensions {
        if e.module == "tailf-common" {
            if e.name == "info" {
                if let Some(ref text) = e.arg {
                    return Some(tuple(vec![
                        atom("doc"),
                        tuple(vec![atom("doc"), list(tagpath.to_vec())]),
                        binary_str(text),
                        int(0),
                        undefined(),
                    ]));
                }
            } else if e.name == "info-html" {
                if let Some(ref text) = e.arg {
                    return Some(tuple(vec![
                        atom("doc"),
                        tuple(vec![atom("doc"), list(tagpath.to_vec())]),
                        binary_str(text),
                        int(F_DOC_IS_HTML as i32),
                        undefined(),
                    ]));
                }
            }
        }
    }
    None
}

/// Compute child flags contributed by cli flags (yanger child_flags/3 cli-based logic).
/// `cli_words`: the [u64; 3] words of the cli_flags value.
/// `is_leaf`: true for leaf/leaf-list/anyxml nodes.
/// These flags are OR-ed into the node's OWN flags (via yanger's add_child_info mechanism)
/// and also propagate to the parent via child_aggregate.
fn compute_cli_child_flags(cli_words: &[u64; 3], node: &SchemaNode) -> u128 {
    // Low 128 bits cover F_CLI_SHOW_NO(0), F_CLI_SHOW_CONFIG(1), F_CLI_SHOW_WITH_DEFAULT(79),
    // F_CLI_CONFIGURE_MODE(105).
    let lo: u128 = (cli_words[0] as u128) | ((cli_words[1] as u128) << 64);
    let mut cf = 0u128;
    if lo & (F_CLI_SHOW_NO | F_CLI_SHOW_WITH_DEFAULT) != 0 {
        cf |= F_CS_CHILD_SHOW_NO_SET;
    }
    let is_leaf = matches!(
        node.kind,
        SchemaNodeKind::Leaf { .. }
            | SchemaNodeKind::AnyXml { .. }
            | SchemaNodeKind::AnyData { .. }
    );
    if is_leaf {
        if lo & F_CLI_SHOW_CONFIG != 0 {
            cf |= F_CS_CHILD_SHOW_CONFIG;
        }
        if lo & F_CLI_CONFIGURE_MODE != 0 {
            cf |= F_CS_CHILD_HAS_RESET;
        }
    }
    // bits 134 (F_CLI_DIFF_AFTER) and 135 (F_CLI_DIFF_DELETE_AFTER) are in words[2] bits 6,7
    if cli_words[2] & ((1u64 << 6) | (1u64 << 7)) != 0 {
        cf |= F_CS_CHILD_HAS_DIFF_DELETE_AFTER;
    }
    cf
}

/// the low 128 bits of cli_flags (for computing my_child_flags), and extra
/// records generated by HasSubstatements cli extensions.
///
/// Returns `(cli_flags_term, cli_words, cli_extra_records)`.
fn compute_cli_flags(extensions: &[ExtensionInstance]) -> (Term, [u64; 3], Vec<Term>) {
    // Use [u64; 3] to represent up to 192 bits (more than enough for bit 144).
    let mut words = [0u64; 3];
    let set_bit = |words: &mut [u64; 3], bit: u32| {
        let word = (bit / 64) as usize;
        let pos = bit % 64;
        if word < 3 {
            words[word] |= 1u64 << pos;
        }
    };
    let mut cli_extra: Vec<Term> = Vec::new();
    for ext in extensions {
        if ext.module != "tailf-common" {
            continue;
        }
        let bit: u32 = match ext.name.as_str() {
            "cli-show-no" => 0,
            "cli-show-config" => 1,
            "cli-mode-name" => 2,
            "cli-mode-name-actionpoint" => 3,
            "cli-add-mode" => 4,
            "cli-suppress-mode" => 5,
            "cli-suppress-table" => 6,
            "cli-suppress-key-abbreviation" => 7,
            "cli-allow-key-abbreviation" => 8,
            "cli-table-legend" => 9,
            "cli-completion-actionpoint" => 10,
            "cli-completion-id" => 11,
            "cli-allow-range" => 12,
            "cli-suppress-range" => 13,
            "cli-allow-wildcard" => 14,
            "cli-suppress-wildcard" => 15,
            "cli-delayed-auto-commit" => 16,
            "cli-preformatted" => 17,
            "cli-enforce-table" => 18,
            "cli-drop-node-name" => 19,
            "cli-compact-syntax" => 20,
            "cli-column-stats" => 21,
            "cli-column-width" => 22,
            "cli-column-align" => 23,
            "cli-incomplete-command" => 24,
            "cli-full-command" => 25,
            "cli-sequence-commands" => 26,
            "cli-incomplete-show-path" => 27,
            "cli-min-keys" => 28,
            "cli-full-show-path" => 29,
            "cli-max-keys" => 30,
            "cli-suppress-show-path" => 31,
            "cli-suppress-show-match" => 32,
            "cli-no-key-completion" => 33,
            "cli-no-match-completion" => 34,
            "cli-compact-stats" => 35,
            "cli-wrap" => 36,
            "cli-width" => 37,
            "cli-delimiter" => 38,
            "cli-prettify" => 39,
            "cli-spacer" => 40,
            "cli-custom-range" => 41,
            "cli-custom-range-actionpoint" => 42,
            "cli-range-type" => 43,
            "cli-show-template" => 44,
            "cli-show-template-legend" => 45,
            "cli-show-template-enter" => 46,
            // bit 47 = F_CLI_DEFAULT_ORDER (deprecated/removed from cli_map)
            "cli-multi-value" => 48,
            "cli-suppress-validation-warning-prompt" => 49,
            "cli-suppress-key-sort" => 50,
            "cli-run-template" => 51,
            "cli-run-template-legend" => 52,
            "cli-run-template-enter" => 53,
            "cli-display-empty-config" => 54,
            "cli-value-display-template" => 55,
            "cli-expose-key-name" => 56,
            "cli-show-order-taglist" => 57,
            "cli-show-order-tag" => 58,
            "cli-break-sequence-commands" => 59,
            "cli-show-template-footer" => 60,
            "cli-run-template-footer" => 61,
            "cli-table-footer" => 62,
            "cli-multi-word-key" => 63,
            "cli-max-words" => 64,
            "cli-autowizard" => 65,
            "cli-suppress-show-conf-path" => 66,
            "cli-key-format" => 67,
            "cli-list-syntax" => 68,
            "cli-suppress-list-no" => 69,
            "cli-suppress-no" => 70,
            "cli-full-no" => 71,
            "cli-incomplete-no" => 72,
            "cli-flat-list-syntax" => 73,
            "cli-flatten-container" => 74,
            "cli-custom-range-enumerator" => 75,
            "cli-reset-siblings" => 76,
            "cli-hide-in-submode" => 77,
            "cli-prefix-key" => 78,
            "cli-show-with-default" => 79,
            "cli-reset-all-siblings" => 80,
            "cli-reset-container" => 81,
            "cli-exit-command" => 82,
            // bit 83 = F_CLI_INFO (x_cli_info, internal pseudo-statement)
            "cli-boolean-no" => 84,
            "cli-optional-in-sequence" => 85,
            "cli-allow-join-with-key" => 86,
            "cli-display-joined" => 87,
            "cli-trim-default" => 88,
            "cli-range-list-syntax" => 89,
            "cli-reversed" => 90,
            "cli-multi-word" => 91,
            "cli-disallow-value" => 92,
            "cli-before-key" => 93,
            "cli-suppress-silent-no" => 94,
            "cli-range-delimiters" => 95,
            "cli-min-column-width" => 96,
            // bit 97 = F_CLI_DONT_CACHE_HIDDEN (internal)
            "cli-oper-info" => 98,
            "cli-custom-error" => 99,
            "cli-remove-before-change" => 100,
            "cli-show-long-obu-diffs" => 101,
            "cli-no-value-on-delete" => 102,
            "cli-no-name-on-delete" => 103,
            "cli-replace-all" => 104,
            "cli-configure-mode" => 105,
            "cli-operational-mode" => 106,
            "cli-auto-legend" => 107,
            "cli-delete-when-empty" => 108,
            "cli-diff-dependency" => 109,
            "cli-ignore-modified" => 110,
            "cli-delete-container-on-delete" => 111,
            "cli-display-separated" => 112,
            "cli-allow-caching" => 113,
            "cli-allow-join-with-value" => 114,
            "cli-explicit-exit" => 115,
            "cli-embed-no-on-delete" => 116,
            "cli-recursive-delete" => 117,
            "cli-no-keyword" => 118,
            "cli-disabled-info" => 119,
            "cli-suppress-shortenabled" => 120,
            "cli-case-sensitive" => 121,
            "cli-case-insensitive" => 122,
            "cli-mount-point" => 123,
            "cli-expose-ns-prefix" => 124,
            "cli-multi-line-prompt" => 125,
            "cli-strict-leafref" => 126,
            "cli-show-obu-comments" => 127,
            "cli-suppress-quotes" => 128,
            "cli-diff-before" => 129,
            "cli-diff-delete-before" => 130,
            "cli-diff-set-before" => 131,
            "cli-diff-create-before" => 132,
            "cli-diff-modify-before" => 133,
            "cli-diff-after" => 134,
            "cli-diff-delete-after" => 135,
            "cli-diff-set-after" => 136,
            "cli-diff-create-after" => 137,
            "cli-diff-modify-after" => 138,
            "cli-reset-full" => 139,
            "cli-only-in-autowizard" => 140,
            "cli-suppress-error-message-value" => 141,
            "cli-suppress-leafref-in-diff" => 142,
            // bit 143 = F_CLI_NONSTRICT_LEAFREF (internal)
            "cli-short-no" => 144,
            _ => u32::MAX,
        };
        if bit != u32::MAX {
            set_bit(&mut words, bit);
        }
        // HasSubstatements extensions: generate {ext_name_r, 0, []} extra record.
        // In yanger_fxs, these trigger a recursive get_cs_cli on the extension's
        // sub-statements. For IOS-XE modules, sub-statements are empty → {ext_r, 0, []}.
        let has_substmts = match ext.name.as_str() {
            "cli-sequence-commands"
            | "cli-incomplete-show-path"
            | "cli-full-show-path"
            | "cli-compact-stats"
            | "cli-custom-range"
            | "cli-custom-range-actionpoint"
            | "cli-show-template"
            | "cli-multi-value"
            | "cli-multi-word-key"
            | "cli-list-syntax"
            | "cli-flat-list-syntax"
            | "cli-custom-range-enumerator"
            | "cli-prefix-key"
            | "cli-exit-command"
            | "cli-boolean-no"
            | "cli-allow-join-with-key"
            | "cli-multi-word"
            | "cli-show-long-obu-diffs"
            | "cli-allow-join-with-value"
            | "cli-completion-actionpoint" => true,
            _ => false,
        };
        if has_substmts {
            // Generate {cli_ext_r, 0, []} record. The key is formed by replacing - with _
            // and appending _r.
            let key_str = ext.name.replace('-', "_") + "_r";
            cli_extra.push(tuple(vec![atom(&key_str), bigint(0), nil()]));
        }
        // String-typed CLI extensions: generate {ext_name, "value"} extra record.
        // Mirrors yanger_fxs conv_cli_arg('string', Arg) which returns a charlist.
        let string_typed = match ext.name.as_str() {
            "cli-mode-name"
            | "cli-mode-name-actionpoint"
            | "cli-table-legend"
            | "cli-completion-id"
            | "cli-delimiter"
            | "cli-spacer" => true,
            _ => false,
        };
        if string_typed {
            if let Some(ref arg) = ext.arg {
                let key_str = ext.name.replace('-', "_");
                cli_extra.push(tuple(vec![atom(&key_str), charlist(arg)]));
            }
        }
    }
    // Convert [u64; 3] to BigInt (little-endian u32 digits for num_bigint).
    let lo = words[0] as u128 | ((words[1] as u128) << 64);
    let hi = words[2];
    let cli_term = if hi == 0 {
        bigint_bigint(BigInt::from(lo))
    } else {
        let combined: BigUint = BigUint::from(lo) + (BigUint::from(hi) << 128u32);
        bigint_bigint(BigInt::from(combined))
    };
    (cli_term, words, cli_extra)
}

fn rw_flags(is_config: bool) -> u128 {
    if is_config {
        F_CS_READ | F_CS_WRITE
    } else {
        F_CS_READ
    }
}

/// Base flags for all CDB-operational nodes (from yanger_fxs cdb_oper_db/1).
fn oper_rw_flags() -> u128 {
    F_CS_READ | F_CS_WRITE_OPERATIONAL | F_CS_IS_CDB | F_CS_WRITE_ALL
}

fn child_aggregate(child_cs: &[&Term]) -> (u128, bool) {
    if child_cs.is_empty() {
        return (0, true); // vacuously all CDB (no children)
    }
    let all_cdb = child_cs.iter().all(|cs| cs_dbm_is_cdb(cs));
    let mut parent_flags: u128 = 0;

    // Flags that propagate directly from child to parent. F_CS_CHILD_LIST propagates
    // transitively through NP containers (yanger: cs_data.childflags includes accumulated
    // grandchild flags for NP containers, so F_CS_CHILD_LIST flows up the tree).
    // F_CS_CHILD_SHOW_NO_SET (bit 6) and other cli-derived child flags also propagate.
    const PROPAGATE_MASK: u128 = F_CS_CHILD_READ_ONLY
        | F_CS_CHILD_READ_WRITE
        | F_CS_CHILD_OPTIONAL
        | F_CS_CHILD_DEFAULT
        | F_CS_CHILD_DELETABLE
        | F_CS_CHILD_LIST
        | F_CS_CHILD_SHOW_NO_SET
        | F_CS_CHILD_SHOW_CONFIG
        | F_CS_CHILD_HAS_RESET
        | F_CS_CHILD_OPER_ACTION
        | F_CS_CHILD_CONF_ACTION
        | F_CS_CHILD_HAS_DIFF_DELETE_AFTER
        | F_CS_CHILD_ORDERED_BY;
    // Structural "non-leaf" type flags: lists and containers accumulate
    // MANDATORY_OR_DEFAULT from their own children but do NOT re-propagate it.
    const NON_LEAF_MASK: u128 = F_CS_IS_LIST | F_CS_IS_CONTAINER | F_CS_IS_NOTIF | F_CS_IS_ACTION;

    for cs in child_cs {
        let cf = extract_cs_flags(cs);
        if cf & (F_CS_IS_ACTION | F_CS_IS_NOTIF) != 0 {
            continue; // Actions and notifications do not contribute child flags to parent.
        }
        // Propagate direct contribution flags (including F_CS_CHILD_LIST for transitive propagation).
        parent_flags |= cf & PROPAGATE_MASK;
        // Propagate MANDATORY_OR_DEFAULT from non-structural children (leaves, leaf-lists, params):
        // - leaf/leaf-list/anyxml already have the flag set (keys, mandatory config leaves, mandatory leaf-lists)
        // - action/notification param leaves don't set the flag on themselves but have exs.min_occurs=1
        // Containers, lists, and other structural nodes (NON_LEAF_MASK) absorb without re-propagating,
        // EXCEPT NP-containers: in yangest, NP containers have min_occurs=1 while P-containers have
        // min_occurs=0. NP-containers pass accumulated MANDATORY_OR_DEFAULT upward (Erlang:
        // reset_child_flags_on_instance only fires for lists and P-containers, NP-containers are transparent).
        if cf & NON_LEAF_MASK == 0 {
            if cf & F_CS_CHILD_MANDATORY_OR_DEFAULT != 0 || extract_exs_min_occurs(cs) > 0 {
                parent_flags |= F_CS_CHILD_MANDATORY_OR_DEFAULT;
            }
        } else if cf & F_CS_IS_CONTAINER != 0 && extract_exs_min_occurs(cs) != 0 {
            // NP-container (min_occurs=1 in yangest): propagate its accumulated MANDATORY_OR_DEFAULT upward.
            // P-containers have min_occurs=0 and are barriers (do NOT propagate).
            parent_flags |= cf & F_CS_CHILD_MANDATORY_OR_DEFAULT;
        }
        // IS_LIST: always contributes CHILD_LIST; DELETABLE only if config (has F_CS_WRITE).
        if cf & F_CS_IS_LIST != 0 {
            parent_flags |= F_CS_CHILD_LIST;
            if cf & F_CS_WRITE != 0 {
                parent_flags |= F_CS_CHILD_DELETABLE;
            }
        }
        // When an immediate child has F_CS_CLI_NAME (tailf:alt-name), set IMMEDIATE_CHILD_HAS_CLI_NAME on parent.
        if cf & F_CS_CLI_NAME != 0 {
            parent_flags |= F_CS_IMMEDIATE_CHILD_HAS_CLI_NAME;
        }
        // Note: IS_LEAF_LIST does NOT contribute F_CS_CHILD_LIST to parent (yanger only checks F_CS_IS_LIST).
    }
    (parent_flags, all_cdb)
}

fn extract_cs_flags(cs_term: &Term) -> u128 {
    // {cs, tagpath, htag, ns, hns, exs, keys, flags, dbm, ...}
    //   0     1      2    3   4    5    6      7      8
    if let Term::Tuple(t) = cs_term {
        if t.elements.len() > 7 {
            return term_to_u128(&t.elements[7]);
        }
    }
    0
}

/// Extract min_occurs from the exs sub-record inside a cs record.
/// exs tuple: {exs, tagpath, typ, prim_type, default, attrs, min_occurs, max_occurs, children, flags, extra}
///              0      1      2       3          4       5        6           7           8       9      10
/// cs tuple:  {cs, tagpath, htag, ns, hns, exs, keys, flags, ...}
///              0     1      2    3   4    5    6      7
fn extract_exs_min_occurs(cs_term: &Term) -> i64 {
    if let Term::Tuple(cs) = cs_term {
        if cs.elements.len() > 5 {
            if let Term::Tuple(exs) = &cs.elements[5] {
                if exs.elements.len() > 6 {
                    match &exs.elements[6] {
                        Term::FixInteger(n) => return n.value as i64,
                        Term::BigInteger(b) => {
                            if let Ok(arr) = <[u8; 8]>::try_from(b.value.to_bytes_le().1.as_slice())
                            {
                                return i64::from_le_bytes(arr);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    0
}

fn cs_dbm_is_cdb(cs_term: &Term) -> bool {
    if let Term::Tuple(t) = cs_term {
        if t.elements.len() > 8 {
            if let Term::Atom(a) = &t.elements[8] {
                return a.name == "cdb";
            }
        }
    }
    false
}

fn term_to_u128(term: &Term) -> u128 {
    match term {
        Term::FixInteger(fi) => fi.value.max(0) as u128,
        Term::BigInteger(bi) => {
            let (sign, bytes) = bi.value.to_bytes_be();
            if sign == num_bigint::Sign::Minus {
                return 0;
            }
            if bytes.len() <= 16 {
                let mut buf = [0u8; 16];
                buf[16 - bytes.len()..].copy_from_slice(&bytes);
                u128::from_be_bytes(buf)
            } else {
                0
            }
        }
        _ => 0,
    }
}

fn update_max_keypath(state: &mut WalkState, depth: u32) {
    if depth > state.max_keypath_length {
        state.max_keypath_length = depth;
    }
}

// ---------------------------------------------------------------------------
// exs.children encoding helpers (mirrors yanger_fxs:mk_exs_children0/5)
// ---------------------------------------------------------------------------

/// Returns true if `children` contains a mandatory choice node.
/// A choice is mandatory when `mandatory=true` and it doesn't have explicit `config=false`.
/// This is used to set `F_CS_CHILD_MANDATORY_CHOICE` on parent CS nodes.
fn has_mandatory_choice_child(children: &[SchemaNode]) -> bool {
    children.iter().any(|ch| {
        if let SchemaNodeKind::Choice { mandatory, .. } = &ch.kind {
            *mandatory && ch.config != Some(false)
        } else {
            false
        }
    })
}

/// Encode a single schema node as an exs.children term.
///
/// - `Choice` → `{choice, Name, [{case, CaseName, [ChildTerms...]}, ...], Default, MinOccurs}`
/// - Everything else → `atom(Name)` (leaf, container, list, leaf-list, anyxml, etc.)
///
/// `is_config` controls `min_occurs` for choice nodes:
///   mandatory choice inside config data → min_occurs=1
///   mandatory choice inside action/notification → min_occurs=0 (config=false)
fn mk_exs_child_term(node: &SchemaNode, is_config: bool, ctx: &ExpansionCtx<'_>) -> Term {
    match &node.kind {
        SchemaNodeKind::Choice {
            default, mandatory, ..
        } => {
            let cases = node.children(ctx); // these are Case nodes
            let case_terms: Vec<Term> = cases
                .iter()
                .map(|case_node| mk_exs_case_term(case_node, is_config, ctx))
                .collect();
            let default_atom = match default {
                Some(d) => atom(d),
                None => atom(""),
            };
            // min_occurs=1 when mandatory=true AND the choice doesn't have explicit config=false.
            // This mirrors yanger's is_mandatory_choice: mandatory=true AND config != false (default=true).
            let min_occurs: i32 = if *mandatory && node.config != Some(false) {
                1
            } else {
                0
            };
            tuple(vec![
                atom("choice"),
                atom(&node.name),
                list(case_terms),
                default_atom,
                int(min_occurs),
            ])
        }
        _ => atom(&node.name),
    }
}

/// Encode a Case node as a `{case, Name, [ChildTerms...]}` term.
/// Children are encoded recursively via `mk_exs_child_term`.
fn mk_exs_case_term(case_node: &SchemaNode, is_config: bool, ctx: &ExpansionCtx<'_>) -> Term {
    let children = case_node.children(ctx);
    let child_terms: Vec<Term> = children
        .iter()
        .map(|ch| mk_exs_child_term(ch, is_config, ctx))
        .collect();
    tuple(vec![atom("case"), atom(&case_node.name), list(child_terms)])
}

/// Build the exs.children terms for a node's children, handling Choice nodes.
/// Direct wrappers around `mk_exs_child_term` for convenience.
fn mk_exs_children_terms(
    children: &[SchemaNode],
    is_config: bool,
    ctx: &ExpansionCtx<'_>,
) -> Vec<Term> {
    children
        .iter()
        .map(|ch| mk_exs_child_term(ch, is_config, ctx))
        .collect()
}

// ---------------------------------------------------------------------------
// Forward-DFS type collection (for ExsTypes ordering)
// ---------------------------------------------------------------------------

/// Scan a raw YANG stmt tree (e.g. a grouping body) for leaves/leaf-lists with inline
/// enumeration types and pre-register them with `undefined` LoadType hash (no load_type).
///
/// This mirrors yanger's `add_enumeration_types()` which processes ALL grouping definitions
/// with `LoadTypeFlags=0` → `LoadType=undefined` → no load_type record. The pre-registration
/// must run BEFORE `collect_types_forward` so that when the schema tree walk encounters a
/// grouping-sourced inline enum it reuses the pre-registered entry instead of registering
/// a new one with IS_ENUMERATION hash + load_type.
fn pre_register_grouping_enum_types(
    stmt: &yangest_core::ast::Stmt,
    module_ns: &str,
    type_gen: &mut TypeGen,
) {
    use yangest_core::ast::{BuiltInKeyword, Keyword};

    match &stmt.keyword {
        Keyword::BuiltIn(BuiltInKeyword::Leaf | BuiltInKeyword::LeafList) => {
            if let Some(type_stmt) = stmt
                .substmts
                .iter()
                .find(|s| matches!(&s.keyword, Keyword::BuiltIn(BuiltInKeyword::Type)))
            {
                if type_stmt.arg.as_deref() == Some("enumeration") {
                    let facets = collect_enum_facets(type_stmt);
                    let misc = collect_enum_misc(type_stmt);
                    type_gen.pre_register_enum_type_no_load(module_ns, &facets, &misc);
                }
            }
        }
        _ => {}
    }
    for sub in &stmt.substmts {
        pre_register_grouping_enum_types(sub, module_ns, type_gen);
    }
}

/// Walk schema nodes in FORWARD DFS order, calling TypeGen for each leaf/leaf-list
/// that generates a t<hash> type.  Must be called BEFORE the reversed-DFS walk_node
/// pass so that TypeGen.entries is in forward-DFS order (matching yanger's prepend
/// accumulation + fxs_write_list-reversal ordering).
fn collect_types_forward(
    node: &SchemaNode,
    module_ns: &str,
    file_module_name: &str,
    ctx: &ExpansionCtx<'_>,
    type_gen: &mut TypeGen,
    module_ns_cache: &std::collections::HashMap<String, String>,
) {
    // Note: we process ALL nodes regardless of module_name.
    // Cross-module nodes (from uses-expansion of a foreign module's grouping) also
    // require inline t<hash> types to be generated in this file's ExsTypes section,
    // matching yanger_fxs behavior which walks all expanded schema nodes.
    match &node.kind {
        SchemaNodeKind::Leaf {
            type_stmt,
            mandatory,
            ..
        } => {
            let type_arg = type_stmt.arg.as_deref().unwrap_or("string");
            let tinfo = type_info_with_registry(type_arg, &node.module_name, ctx.registry);
            // Determine load_flags (same logic as in walk_node Leaf handler)
            let is_key = false; // key status not tracked here — conservatively false
            let is_mandatory = *mandatory;
            let load_flags = if let Some(ref td_mod) = tinfo.typedef_defining_module {
                let is_builtin_mod = matches!(
                    td_mod.as_str(),
                    "ietf-inet-types" | "ietf-yang-types" | "tailf-common" | "tailf-inet-types"
                );
                let from_other = td_mod != file_module_name;
                if !is_builtin_mod && (from_other || node.module_name != file_module_name) {
                    let mut f = F_LOAD_FXS_GET_SUPPRESS_ECHO | F_LOAD_FXS_GET_TYPE_INFO;
                    if !is_key && !is_mandatory {
                        if tinfo.extra_exs_flags & crate::types::F_EXS_IS_ENUMERATION != 0 {
                            f |= F_LOAD_FXS_PARSE_DEFAULT;
                        } else {
                            f |= F_LOAD_FXS_GET_DEFAULT;
                        }
                    }
                    f
                } else {
                    0
                }
            } else {
                0
            };
            // Generate inline constraint types only when there's no cross-module typedef.
            if load_flags == 0 && !tinfo.is_typedef {
                maybe_generate_leaf_type(
                    type_gen,
                    module_ns,
                    type_arg,
                    tinfo.exs_type,
                    type_stmt,
                    &node.module_name,
                    ctx.registry,
                    module_ns_cache,
                    0, // direct leaf: use type-specific default
                    is_key || is_mandatory,
                );
            }
        }

        SchemaNodeKind::LeafList {
            type_stmt,
            min_elements,
            max_elements,
            ..
        } => {
            let type_arg = type_stmt.arg.as_deref().unwrap_or("string");
            let tinfo = type_info_with_registry(type_arg, &node.module_name, ctx.registry);
            let ll_load_flags = if let Some(ref td_mod) = tinfo.typedef_defining_module {
                let is_builtin_mod = matches!(
                    td_mod.as_str(),
                    "ietf-inet-types" | "ietf-yang-types" | "tailf-common" | "tailf-inet-types"
                );
                let from_other = td_mod != file_module_name;
                if !is_builtin_mod && (from_other || node.module_name != file_module_name) {
                    F_LOAD_FXS_GET_DEFAULT | F_LOAD_FXS_GET_SUPPRESS_ECHO | F_LOAD_FXS_GET_TYPE_INFO
                } else {
                    0
                }
            } else {
                0
            };
            if ll_load_flags == 0 {
                let all_flags = compute_leaf_list_all_load_type_flags(
                    type_arg,
                    type_stmt,
                    &tinfo,
                    &node.module_name,
                    ctx.registry,
                );
                // For inline enum/union/bits leaf-lists, generate the type first, then use it as base.
                let base_type_ref = if !tinfo.is_typedef
                    && (type_arg == "enumeration" || type_arg == "union" || type_arg == "bits")
                {
                    maybe_generate_leaf_type(
                        type_gen,
                        module_ns,
                        type_arg,
                        tinfo.exs_type,
                        type_stmt,
                        &node.module_name,
                        ctx.registry,
                        module_ns_cache,
                        0, // direct leaf-list: use type-specific default
                        *min_elements > 0,                    )
                    .0
                } else {
                    tinfo.exs_type
                };
                let (base_ns, base_name) = exs_type_ns_name(&base_type_ref);
                if !base_ns.is_empty() {
                    let list_ref =
                        type_gen.get_or_create_list_type(module_ns, base_ns, base_name, all_flags);
                    let (list_ns, list_name) = exs_type_ns_name(&list_ref);
                    let min = *min_elements;
                    let max = *max_elements;
                    type_gen.get_or_create_unique_list_type(
                        module_ns, list_ns, list_name, min, max, all_flags,
                    );
                }
            }
        }

        SchemaNodeKind::Rpc { .. } | SchemaNodeKind::Action { .. } => {
            for child in node.input_children(ctx) {
                collect_types_forward(&child, module_ns, file_module_name, ctx, type_gen, module_ns_cache);
            }
            for child in node.output_children(ctx) {
                collect_types_forward(&child, module_ns, file_module_name, ctx, type_gen, module_ns_cache);
            }
        }

        _ => {
            for child in node.children(ctx) {
                collect_types_forward(&child, module_ns, file_module_name, ctx, type_gen, module_ns_cache);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Typedef record generation
// ---------------------------------------------------------------------------

/// Generate `#exs_type{}` and `#load_type{}` records for all typedefs defined
/// in `module`.  Also populates `type_gen` with any t<hash> types that arise
/// from inline union member types (e.g. `type union { type uint32 { range ... } }`).
/// Returns (exs_types, load_types).
fn generate_typedef_records(
    module: &CompiledModule,
    registry: &ModuleRegistry,
    type_gen: &mut TypeGen,
    module_ns_cache: &std::collections::HashMap<String, String>,
) -> (Vec<Term>, Vec<Term>) {
    use yangest_core::ast::{BuiltInKeyword, Keyword};

    let mut exs_types: Vec<Term> = Vec::new();
    let mut load_types: Vec<Term> = Vec::new();
    let module_ns = &module.namespace;

    // Sort typedefs in reverse alphabetical order (Z-to-A), matching yanger's gb_trees
    // fold ordering (which also processes keys in sorted order and prepends to list).
    let mut sorted_typedefs: Vec<(&String, _)> = module.typedefs.iter().collect();
    sorted_typedefs.sort_by(|(a, _), (b, _)| b.cmp(a));

    for (name, typedef) in sorted_typedefs {
        let base_arg = typedef.type_stmt.arg.as_deref().unwrap_or("string");

        match base_arg {
            "enumeration" => {
                let mut enum_vals: Vec<(String, Option<String>, i64)> = Vec::new();
                let mut next_val: i64 = 0;
                for sub in &typedef.type_stmt.substmts {
                    if matches!(&sub.keyword, Keyword::BuiltIn(BuiltInKeyword::EnumStmt)) {
                        let enum_name = sub.arg.clone().unwrap_or_default();
                        let val = sub
                            .get_substmt(BuiltInKeyword::Value)
                            .and_then(|v| v.arg.as_deref())
                            .and_then(|s| s.parse::<i64>().ok())
                            .unwrap_or(next_val);
                        next_val = val + 1;
                        // Read optional tailf:code-name extension
                        let code_name = sub
                            .substmts
                            .iter()
                            .find(|s| match &s.keyword {
                                Keyword::Extension { module, name } => {
                                    module == "tailf-common" && name == "code-name"
                                }
                                Keyword::ExtensionPrefixed { prefix: _, name } => {
                                    name == "code-name"
                                }
                                _ => false,
                            })
                            .and_then(|s| s.arg.clone());
                        enum_vals.push((enum_name, code_name, val));
                    }
                }
                // Reverse declaration order (yanger builds enum list with prepend, so last-declared is first)
                enum_vals.reverse();

                let enum_terms: Vec<Term> = enum_vals
                    .iter()
                    .map(|(n, code_name, v)| {
                        let cn_term = match code_name {
                            Some(cn) => crate::terms::binary_str(cn),
                            None => atom("false"),
                        };
                        tuple(vec![
                            atom("enumeration"),
                            crate::terms::binary_str(n),
                            cn_term,
                            int(*v as i32),
                        ])
                    })
                    .collect();

                let xsd_string = tuple(vec![
                    atom("http://www.w3.org/2001/XMLSchema"),
                    atom("string"),
                ]);
                let derivation = tuple(vec![
                    atom("restriction"),
                    xsd_string.clone(),
                    list(enum_terms.clone()),
                ]);

                exs_types.push(tuple(vec![
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
                    int(0),
                ]));

                // Resolve enum default value if the typedef has one.
                // Encodes as (BENUMHASH=28, integer_value).
                let (default_str_term, default_term) = if let Some(dflt) = &typedef.default {
                    let int_val = enum_vals
                        .iter()
                        .find(|(n, _, _)| n == dflt)
                        .map(|(_, _, v)| *v as i32)
                        .unwrap_or(0);
                    (
                        crate::terms::binary_str(dflt),
                        tuple(vec![int(28), int(int_val)]),
                    )
                } else {
                    (undefined(), undefined())
                };

                load_types.push(tuple(vec![
                    atom("load_type"),
                    atom(name),
                    xsd_string,
                    int(2), // F_LOAD_FXS_IS_ENUMERATION = 1 << 1
                    default_str_term,
                    default_term,
                    atom("string"),
                    list(enum_terms.clone()),
                ]));
            }

            "bits" => {
                let fields = collect_bits_fields(&typedef.type_stmt);
                if fields.is_empty() {
                    continue;
                }
                let max_pos = fields.iter().map(|(p, _)| *p).max().unwrap_or(0);
                let size = bits_type_size(max_pos);
                let primitive = bits_primitive_atom(max_pos);

                let field_terms: Vec<Term> = fields
                    .iter()
                    .map(|(pos, n)| {
                        tuple(vec![
                            int(*pos as i32),
                            Term::from(eetf::ByteList::from(n.as_str())),
                        ])
                    })
                    .collect();

                let derivation = tuple(vec![
                    atom("bits"),
                    list(field_terms.clone()),
                    int(size as i32),
                ]);
                // Typedef bits exs_type has flags=0 (not generated/inline)
                exs_types.push(tuple(vec![
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
                    int(0),
                ]));

                // load_type: flags=F_LOAD_FXS_IS_BITS(64), primitive=bits_type_xx, data={bits,fields,size}
                load_types.push(tuple(vec![
                    atom("load_type"),
                    atom(name),
                    undefined(), // base (bits has no xsd base type)
                    int(64),     // F_LOAD_FXS_IS_BITS
                    undefined(),
                    undefined(),
                    atom(primitive),
                    tuple(vec![
                        atom("bits"),
                        list(field_terms.clone()),
                        int(size as i32),
                    ]),
                ]));
            }

            "union" => {
                let any_cross_module = union_has_cross_module_member(
                    &typedef.type_stmt,
                    &module.key.name,
                    registry,
                );
                // For typedef unions, yanger's add_exs_type:
                // 1. Calls mk_union_derivation(LoadFlags) with LoadFlags = get_load_flags() = 0
                //    for inline builtin union members.  Member restriction type hashes use
                //    LoadTypeFlags = 0 → undefined load_type in phash2.
                // 2. Separately, LoadTypeFlags for the typedef's OWN load_type.flags is computed
                //    from get_type_info(LoadFlags, Type) = IS_UNION (since type_spec is union_type_spec)
                //    via LoadIsFlags, regardless of the member member_load_flags.
                //
                // So:  union_flags (for the typedef's own load_type.flags) = IS_UNION or GET_TYPE_INFO
                //      member_load_flags (for restriction type hashes of members) = 0 (same-module)
                //      or GET_TYPE_INFO|... (cross-module members from get_load_flags fold)
                let union_flags = if any_cross_module {
                    F_LOAD_FXS_GET_DEFAULT | F_LOAD_FXS_GET_TYPE_INFO
                } else {
                    F_LOAD_FXS_IS_UNION
                };
                // Member restriction types are hashed with LoadTypeFlags from get_load_flags(),
                // NOT from the union's own flags. For inline builtins, that is 0.
                // For cross-module typedef members, it's GET_TYPE_INFO|..., but those members
                // are handled as type_refs (not new restriction types), so member_load_flags=0
                // is safe for inline restriction hashing in both cases.
                let member_load_flags = 0u32;
                let pre_count = type_gen.len();
                let (member_refs, member_primitives) =
                    collect_inline_union_members(
                        &typedef.type_stmt,
                        &module.key.name,
                        module_ns,
                        registry,
                        module_ns_cache,
                        type_gen,
                        member_load_flags,
                    );

                if member_refs.is_empty() {
                    continue;
                }

                let derivation = tuple(vec![atom("union"), list(member_refs.clone())]);
                exs_types.push(tuple(vec![
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
                    int(0),
                ]));
                // Interleave any inline member types (restriction hashes) generated during
                // collect_inline_union_members immediately after this union typedef's named record.
                // This matches yanger's output ordering where member type records follow their
                // parent union typedef declaration, not deferred to the end of all typedef records.
                let post_count = type_gen.len();
                if post_count > pre_count {
                    exs_types.extend(type_gen.exs_type_terms_range(pre_count, post_count));
                    load_types.extend(type_gen.load_type_terms_range(pre_count, post_count));
                }

                let primitive = if any_cross_module {
                    undefined()
                } else {
                    list(member_primitives)
                };
                load_types.push(tuple(vec![
                    atom("load_type"),
                    atom(name),
                    list(member_refs),
                    int(union_flags as i32),
                    undefined(),
                    undefined(),
                    primitive,
                    undefined(),
                ]));
            }

            other => {
                // Resolve the base type info.
                // For cross-module derived typedefs (prefix:name), use registry; for builtins use directly.
                let base_info =
                    if other.contains(':') || (!is_yang_builtin(other) && !other.is_empty()) {
                        crate::types::type_info_with_registry(other, &module.key.name, registry)
                    } else {
                        crate::types::resolve_builtin_type(other)
                    };

                let xsd_type = base_info.exs_type.clone();

                // Parse any restriction facets (range, length, pattern) from the type's substmts.
                let facets_eetf = parse_typedef_facets(other, &typedef.type_stmt.substmts);

                // Compute exs_type flags: F_EXS_TYPE_HAS_YANG_PATTERN(4) if any pattern present.
                let has_pattern = typedef
                    .type_stmt
                    .substmts
                    .iter()
                    .any(|s| matches!(&s.keyword, Keyword::BuiltIn(BuiltInKeyword::Pattern)));
                let exs_type_flags: u32 = if has_pattern { 4 } else { 0 };

                let derivation = tuple(vec![
                    atom("restriction"),
                    xsd_type.clone(),
                    list(facets_eetf),
                ]);
                exs_types.push(tuple(vec![
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
                    int(exs_type_flags as i32),
                ]));

                // load_type flags: 0 for simple types; 8200 for leafref (IS_LEAFREF | HAS_LEAFREF_DATA).
                // For typedefs deriving from a cross-module non-builtin type, set GET flags:
                // F_LOAD_FXS_GET_DEFAULT(128) | F_LOAD_FXS_GET_SUPPRESS_ECHO(512) | F_LOAD_FXS_GET_TYPE_INFO(1024) = 1664
                //
                // IS_UNION (4) is set when the base typedef resolves to a union type AND the base
                // module is "builtin" (ietf-inet-types etc.) -- in yanger, mk_extra_types() expands
                // the union members making TypeSpec != BaseTypeSpec, so identity_derivation_flag()
                // doesn't fire; instead get_type_info() sees union_type_spec → IS_UNION.
                //
                // IS_EMPTY (32) is set when the base typedef resolves to the empty type.
                //
                // IS_IDENTITY_DERIVATION (65536) is set for ANY typedef that is a pure derivation
                // (no restriction substmts added), where the base is a typedef (not a builtin), and
                // the base is NOT a union type (union types get IS_UNION instead).
                let (load_flags, load_base, load_data) = if other == "leafref" {
                    (int(8200), undefined(), undefined())
                } else if other == "empty" {
                    // Direct use of empty type (not through a typedef).
                    (int(F_LOAD_FXS_IS_EMPTY as i32), xsd_type, undefined())
                } else {
                    // Detect identity derivation: no restriction facets in the type's substmts.
                    let has_facets = typedef.type_stmt.substmts.iter().any(|s| {
                        matches!(
                            &s.keyword,
                            Keyword::BuiltIn(
                                BuiltInKeyword::Range
                                    | BuiltInKeyword::Pattern
                                    | BuiltInKeyword::Length
                                    | BuiltInKeyword::FractionDigits
                                    | BuiltInKeyword::EnumStmt
                                    | BuiltInKeyword::Bit
                            )
                        )
                    });
                    let get_flags = if let Some(ref td_mod) = base_info.typedef_defining_module {
                        let is_builtin_mod = matches!(
                            td_mod.as_str(),
                            "ietf-inet-types"
                                | "ietf-yang-types"
                                | "tailf-common"
                                | "tailf-inet-types"
                        );
                        // Detect if the base typedef ultimately resolves to a union or empty type.
                        let base_is_union = is_union_base(other, &module.key.name, registry);
                        let base_is_empty =
                            !base_is_union && is_empty_base(other, &module.key.name, registry);
                        let mut f: u32 = 0;
                        if base_is_union && is_builtin_mod {
                            // Builtin-module union types: yanger expands the union members at
                            // compile time so TypeSpec != BaseTypeSpec → IS_UNION.
                            // IS_IDENTITY_DERIVATION is also set when there are no facets (pure derivation).
                            f |= F_LOAD_FXS_IS_UNION;
                            if !has_facets {
                                f |= F_LOAD_FXS_IS_IDENTITY_DERIVATION;
                            }
                        } else if base_is_empty {
                            f |= F_LOAD_FXS_IS_EMPTY;
                        } else {
                            // GET flags only for cross-module non-builtin types.
                            if !is_builtin_mod && td_mod.as_str() != module.key.name.as_str() {
                                // Use PARSE_DEFAULT when the typedef has its own default value,
                                // otherwise GET_DEFAULT.
                                let default_flag = if typedef.default.is_some() {
                                    F_LOAD_FXS_PARSE_DEFAULT
                                } else {
                                    F_LOAD_FXS_GET_DEFAULT
                                };
                                f |= default_flag | F_LOAD_FXS_GET_SUPPRESS_ECHO | F_LOAD_FXS_GET_TYPE_INFO;
                            }
                            // IS_IDENTITY_DERIVATION for pure derivations of non-union typedefs.
                            if !has_facets {
                                f |= F_LOAD_FXS_IS_IDENTITY_DERIVATION;
                            }
                        }
                        f as i32
                    } else {
                        0
                    };
                    (int(get_flags), xsd_type, undefined())
                };
                load_types.push(tuple(vec![
                    atom("load_type"),
                    atom(name),
                    load_base,
                    load_flags,
                    undefined(),
                    undefined(),
                    base_info.primitive_type,
                    load_data,
                ]));
            }
        }
    }

    (exs_types, load_types)
}

/// Returns true if `name` is a YANG built-in type that does not need typedef lookup.
fn is_yang_builtin(name: &str) -> bool {
    matches!(
        name,
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
            | "binary"
            | "decimal64"
            | "empty"
            | "enumeration"
            | "union"
            | "bits"
            | "leafref"
            | "identityref"
            | "instance-identifier"
    )
}

/// Parse restriction facets (range, length, pattern) from a type stmt's sub-statements,
/// returning eetf Terms suitable for the `[facets]` list in a `restriction` derivation.
fn parse_typedef_facets(type_arg: &str, substmts: &[yangest_core::ast::Stmt]) -> Vec<Term> {
    use yangest_core::ast::{BuiltInKeyword, Keyword};
    let mut facets: Vec<Term> = Vec::new();

    // Determine the XSD integer type tag for range encoding (needed for IntBound)
    let xsd_tag = yang_int_to_xsd_info(type_arg)
        .map(|(_, tag)| tag)
        .unwrap_or(8);

    for sub in substmts {
        match &sub.keyword {
            Keyword::BuiltIn(BuiltInKeyword::Range) => {
                if let Some(arg) = sub.arg.as_deref() {
                    let ranges = parse_range_bounds(arg, xsd_tag);
                    if !ranges.is_empty() {
                        facets.push(range_facet_eetf(&ranges));
                    }
                }
            }
            Keyword::BuiltIn(BuiltInKeyword::Length) => {
                if let Some(arg) = sub.arg.as_deref() {
                    let ranges = parse_length_ranges(arg);
                    if !ranges.is_empty() {
                        facets.push(length_facet_eetf(&ranges));
                    }
                }
            }
            Keyword::BuiltIn(BuiltInKeyword::Pattern) => {
                if let Some(arg) = sub.arg.as_deref() {
                    facets.push(crate::thash::pattern_facet_eetf(arg.as_bytes()));
                }
            }
            _ => {}
        }
    }
    facets
}
