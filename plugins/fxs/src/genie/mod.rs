//! Vendored subset of `erl_dict_genie` (https://github.com/lmth/erl_dict_genie).
//!
//! Replicates the `dict:fold/3` traversal order produced by Erlang's `dict`
//! module, without running Erlang.  Used to sort `#hash{}` records in the FXS
//! output so that the MD5 checksum matches yanger's output byte-for-byte.
//!
//! Public API: [`dict_fold_order`].

// The vendored modules support more term types than we use (atoms only);
// suppress the resulting dead-code warnings.
#[allow(dead_code)]
pub mod term;
#[allow(dead_code)]
mod phash;
mod dict;

use dict::ErlangDict;
use term::Term;

/// Given a slice of Erlang term keys in **insertion order** (the order they
/// were passed to `dict:store/3`), returns their indices in `dict:fold/3`
/// traversal order.
///
/// `fxs_write_dict` in `confd_rt_tools.erl` builds a list by folding with
/// prepend (`[V|Acc]`) and then passes it to `fxs_write_list` which reverses
/// it.  The net effect is that the written list order equals the fold order
/// (high bucket → low bucket, insertion order within a bucket).
///
/// # Panics
///
/// Panics if `keys` contains duplicates (dict keys must be unique).
pub fn dict_fold_order(keys: &[String]) -> Vec<usize> {
    let terms: Vec<Term> = keys
        .iter()
        .map(|k| Term::Atom(k.as_bytes().to_vec()))
        .collect();
    dict_fold_order_terms(&terms)
}

/// Same as [`dict_fold_order`] but accepts arbitrary Erlang term keys.
pub fn dict_fold_order_terms(terms: &[Term]) -> Vec<usize> {
    let mut d = ErlangDict::new();
    for t in terms {
        d.store(t.clone());
    }

    // Build a lookup: index by position in the input slice.
    // Since dict fold returns full terms, we match them back by position.
    let pos: std::collections::HashMap<usize, usize> = terms
        .iter()
        .enumerate()
        .map(|(i, _)| (i, i))
        .collect();
    let _ = pos; // unused

    // We need to match fold output back to input indices.
    // Use a simpler approach: assign each term an index and find it in fold output.
    // Since terms may not be easily compared without PartialEq on Term (they are),
    // we rely on the fold_order returning terms in the fold order.
    d.fold_order_indices(terms)
}
