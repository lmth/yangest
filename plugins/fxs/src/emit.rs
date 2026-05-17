//! Top-level FXS file emitter.

use std::io::Write;
use std::sync::Arc;

use yangest_core::ast::Keyword;
use yangest_core::compiler::{CompiledModule, ExpansionCtx, ModuleRegistry};

use crate::header::{build_fxs_header_final, build_fxs_header_placeholder, build_yang_header};
use crate::schema::walk_module;
use crate::serial::FxsWriter;
use crate::terms::{atom, binary, binary_str, charlist, int, list, make_callpoint_info, nil, tuple, undefined};

use eetf::Term;

const FXS_VSN: &str = "8.8_1";
const FXS_SECTION_YANG: u8 = 1;

/// Emit an FXS file for `module` into `out`.
///
/// If `no_yang_source` is true (mirrors `--fxs-no-yang-source`), the YANG
/// source text is not embedded in the file.
pub fn emit_fxs(
    module: &Arc<CompiledModule>,
    registry: &ModuleRegistry,
    ctx: &ExpansionCtx<'_>,
    out: &mut dyn Write,
    no_yang_source: bool,
) -> std::io::Result<()> {
    // Decide whether to include the YANG source section.
    // Mirrors yanger_fxs's DoAddYang logic:
    // - suppressed by --fxs-no-yang-source
    // - never included for NCS device models (tailf:ncs-device-type)
    let is_ncs_device_model = module.stmt.substmts.iter().any(|s| {
        matches!(&s.keyword, Keyword::Extension { module, name }
            if module == "tailf-common" && name == "ncs-device-type")
    });
    let do_add_yang = !no_yang_source && !is_ncs_device_model && module.source_path.is_some();

    // 1. Walk the schema tree
    let walk = walk_module(module, ctx);

    // 2. Build the yang_header term
    let yang_header = build_yang_header(module, ctx, registry);

    // 3. Model sizes: (max_keypath_length, max_key_tuple_size)
    let model_sizes = tuple(vec![
        int(walk.max_keypath_length as i32),
        int(walk.max_key_tuple_size as i32),
    ]);

    // 4. Build dummy sections for placeholder (same structure as final, but
    //    with zero byte offsets so the ETF encoding is the same size).
    let dummy_sections = make_sections_term(do_add_yang, 0);

    // Collect unique target namespaces from load_augment records for fxs_header.augments.
    // Each load_augment tuple: {load_augment, tagpath, htag, target_ns, target_hns, ...}
    // target_ns is at index 3 (0-based).
    let augments_list: Term = {
        let mut seen = std::collections::HashSet::new();
        let mut ns_atoms: Vec<Term> = Vec::new();
        for rec in &walk.load_augment_records {
            if let Term::Tuple(tup) = rec {
                if tup.elements.len() > 3 {
                    let ns_term = tup.elements[3].clone();
                    if let Term::Atom(ref a) = ns_term {
                        if seen.insert(a.name.clone()) {
                            ns_atoms.push(ns_term);
                        }
                    }
                }
            }
        }
        list(ns_atoms)
    };

    // 5. Build placeholder fxs_header
    let placeholder = build_fxs_header_placeholder(
        module,
        registry,
        yang_header,
        model_sizes,
        augments_list,
        walk.has_cdb,
        walk.has_cdb_oper,
        dummy_sections,
    );

    // 6. Write {FXS_VSN, fxs_header_placeholder} (uncompressed)
    let header_term = tuple(vec![charlist(FXS_VSN), placeholder.clone()]);

    let mut writer = FxsWriter::new();
    writer.write_magic();
    let _header_pos = writer.write_header(&header_term);

    // 7. Write data sections (compressed, updating MD5)

    // Partition cs_records into CDB (→ CsCdbL) and non-CDB (→ CsL).
    // Both lists preserve pre-order; fxs_write_list will reverse each chunk
    // when serializing, matching yanger's fxs_write_list reversal behaviour.
    let (cs_cdb, cs_non_cdb): (Vec<_>, Vec<_>) = walk
        .cs_records
        .into_iter()
        .partition(|cs| cs_term_is_cdb(cs));

    // ExsTypes ordering (matching yanger reference behavior):
    //   1. Schema-walk inline t<hash> types in REVERSE DFS order (last encountered first)
    //   2. Named typedef exs_type records (Z-to-A, matching yanger's gb_trees fold order)
    //      with inline union-member t<hash> types interleaved immediately after their parent.
    // write_list reverses items within each chunk; fxs-print un-reverses, so fxs-print
    // shows items in INPUT order. The DFS-reversed order matches the reference.
    let mut all_exs: Vec<Term> = walk.generated_exs_type_records.into_iter().rev().collect();
    all_exs.extend(walk.exs_type_records);
    all_exs.extend(walk.typedef_inline_exs_type_records);
    writer.write_list(&all_exs);  // ExsTypes
    // LoadTypes: schema-walk inline load_types (reversed DFS) + named typedef load_types
    // (with union-member inline load_types interleaved after their parent typedef's load_type).
    let mut all_load_types: Vec<Term> = walk.generated_load_type_records.into_iter().rev().collect();
    all_load_types.extend(walk.load_type_records);
    writer.write_list(&all_load_types);  // LoadTypes
    writer.write_list(&walk.load_augment_records);   // AugL
    writer.write_list(&cs_cdb);  // CsCdbL
    writer.write_list(&walk.identity_records);   // Identities
    writer.mark_cdb_done();       // CDB checksum covers everything above

    // CsL: non-CDB cs records only
    writer.write_list(&cs_non_cdb);

    // Misc2: doc record if module has a description, plus action records for RPC/action nodes,
    // plus doc entries for enum/bit values with tailf:info.
    let misc2 = build_misc2(module, &walk.misc_records, &walk.type_doc_records);
    writer.write_list(&misc2);
    writer.write_dict(&walk.hash_records); // HashDict

    // callpoint_info: one entry per unique actionpoint name, sorted (mirrors yanger's
    // lists:sort(callpoint_db:tab2list(CallpointDb)) which deduplicates via ETS keys).
    let mut sorted_ap_names: Vec<String> = walk.actionpoint_names.clone();
    sorted_ap_names.sort();
    sorted_ap_names.dedup();
    let callpoint_info_list: Vec<Term> = sorted_ap_names
        .iter()
        .map(|name| {
            let ap_key = tuple(vec![atom("ap"), atom(name)]);
            tuple(vec![ap_key, atom("datamodel")])
        })
        .collect();
    let callpoint_info = make_callpoint_info(list(callpoint_info_list));
    writer.write_list(&[callpoint_info]);

    // End of main data section
    writer.write_end_marker();

    // 8. Optional YANG source section (after main end marker, before final end marker)
    let yang_section_pos = writer.current_pos();
    if do_add_yang {
        if let Some(ref path) = module.source_path {
            let source = std::fs::read(path).map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!("cannot read YANG source {:?}: {}", path, e),
                )
            })?;
            writer.write_yang_file(
                &module.key.name,
                module.key.revision.as_deref(),
                &source,
            );
        }
        // End of YANG section
        writer.write_end_marker();
    }

    // Final end-section marker (always present)
    writer.write_end_marker();

    // 9. Finish: patch header with final checksums and sections
    let sections = make_sections_term(do_add_yang, yang_section_pos);

    let final_bytes = writer.finish(|cdb_checksum, full_checksum| {
        let final_header =
            build_fxs_header_final(&placeholder, cdb_checksum, full_checksum, sections);
        tuple(vec![charlist(FXS_VSN), final_header])
    });

    out.write_all(&final_bytes)?;
    Ok(())
}

/// Build the `sections` list for the fxs_header.
///
/// If `do_add_yang` is true, produces `[{<<1>>, <<pos:32>>}]`.
/// Otherwise produces `[]`.
fn make_sections_term(do_add_yang: bool, yang_pos: u32) -> eetf::Term {
    if do_add_yang {
        let pos_bytes = yang_pos.to_be_bytes();
        list(vec![tuple(vec![
            binary(vec![FXS_SECTION_YANG]),
            binary(pos_bytes.to_vec()),
        ])])
    } else {
        nil()
    }
}

/// Build the Misc2 section contents: a doc record if the module has a description,
/// followed by action records for each RPC/action node (default nyi callback),
/// plus doc records for enum/bit values with tailf:info (from type Misc).
///
/// NOTE: `write_list` reverses its input, so we push records in REVERSE final order:
/// type_doc_records (reversed, so forward-DFS order appears first after reversal),
/// then action records, then doc — so after reversal the file has:
/// [first_type_doc, ..., last_type_doc, doc, action, ...]
/// which matches Erlang's GS.misc = [module_doc | type_docs_via_prepend] with fxs_write_list reversal.
fn build_misc2(
    module: &Arc<CompiledModule>,
    misc_records: &[eetf::Term],
    type_doc_records: &[eetf::Term],
) -> Vec<eetf::Term> {
    use yangest_core::ast::BuiltInKeyword;
    let mut records = Vec::new();
    // type_doc_records are in forward DFS order; push them in REVERSE so that after
    // write_list's reversal they appear in forward DFS order (earliest first).
    for doc_rec in type_doc_records.iter().rev() {
        records.push(doc_rec.clone());
    }
    // misc_records (node doc + action records) are accumulated in reversed-DFS order during
    // walk_node. Push them as-is; write_list's reversal will then yield forward-DFS order in file.
    records.extend_from_slice(misc_records);
    if let Some(desc_stmt) = module.stmt.get_substmt(BuiltInKeyword::Description) {
        if let Some(desc) = &desc_stmt.arg {
            let doc_record = tuple(vec![
                atom("doc"),
                tuple(vec![atom("doc"), nil()]),
                binary_str(desc),
                int(0),
                undefined(),
            ]);
            records.push(doc_record);
        }
    }
    records
}


///
/// A `#cs{}` tuple has its `dbm` field at index 8 (0-based).
/// Field order from cs.hrl: key=0, tagpath=1, htag=2, ns=3, hns=4, exs=5,
/// keys=6, flags=7, dbm=8, ...
fn cs_term_is_cdb(cs: &eetf::Term) -> bool {
    if let eetf::Term::Tuple(t) = cs {
        if t.elements.len() > 8 {
            if let eetf::Term::Atom(a) = &t.elements[8] {
                return a.name == "cdb";
            }
        }
    }
    false
}
