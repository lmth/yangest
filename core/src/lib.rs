// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
pub mod annindex;
pub mod astannindex;
pub mod ast;
pub mod compiler;
pub mod cursor;
pub mod depgraph;
pub mod diagnostics;
pub mod devindex;
pub mod grammar;
pub mod parser;
pub mod plugin;
pub mod types_registry;
/// Encounter-order schema walker for byte-faithful backends. Retired from core's
/// critical path and gated behind the `experimental-walker` feature — it has no
/// in-tree consumer. See `docs/core-walker-design.md` (STATUS banner) for why.
#[cfg(feature = "experimental-walker")]
pub mod walker;
pub mod xpath;

pub use compiler::ExpansionCtx;
