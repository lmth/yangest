// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
pub mod annindex;
pub mod astannindex;
pub mod ast;
pub mod compiler;
pub mod cursor;
pub mod depgraph;
pub mod devindex;
pub mod grammar;
pub mod parser;
pub mod plugin;
pub mod types_registry;
pub mod xpath;

pub use compiler::ExpansionCtx;
