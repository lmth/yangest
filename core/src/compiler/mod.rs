// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Schema compiler (COMPILE phase).
//!
//! Expands groupings, compiles schema nodes, collects augments, and applies
//! deviations to one module at a time.

mod compile;
mod expansion;
mod types;

pub use compile::{compile_module, expand_children, find_child_in_raw};
pub use expansion::ExpansionCtx;
pub use types::*;
