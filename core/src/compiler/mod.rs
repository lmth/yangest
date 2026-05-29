// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Schema compiler (COMPILE phase).
//!
//! Expands groupings, compiles schema nodes, collects augments, and applies
//! deviations to one module at a time.

mod compile;
mod expansion;
mod types;

pub use compile::{compile_module, expand_children, expand_children_all, expand_children_and_all, find_child_in_raw, walk_path_no_clone, walk_path_with_siblings, fold_path_no_clone, walk_path_collecting_intermediates, raw_children_of_kind};
pub use expansion::ExpansionCtx;
pub use types::*;
