// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Compile-time extension grammar declarations.
//!
//! Plugins that understand YANG extensions register static grammar rules via
//! [`Plugin::yang_grammar`]. Rules are collected at startup into a
//! [`GrammarRegistry`] that the compiler uses during module compilation.
//!
//! All string and slice fields in grammar types use `'static` lifetimes so
//! rules can be declared as top-level `static` constants in plugin crates —
//! no heap allocation at runtime for the grammar declarations themselves.
//!
//! # Example
//!
//! ```rust,ignore
//! use yangest_core::grammar::{ExtensionGrammar, ArgType, SubstmtSpec, Cardinality, GrammarKeyword};
//! use yangest_core::ast::BuiltInKeyword;
//!
//! static MY_GRAMMAR: &[ExtensionGrammar] = &[
//!     ExtensionGrammar {
//!         module: "my-extensions",
//!         name: "purpose",
//!         parents: &[BuiltInKeyword::Container, BuiltInKeyword::Leaf],
//!         arg: Some(ArgType::String),
//!         substmts: &[],
//!     },
//! ];
//!
//! impl Plugin for MyPlugin {
//!     fn yang_grammar(&self) -> &'static [ExtensionGrammar] { MY_GRAMMAR }
//! }
//! ```

use std::collections::HashMap;

use crate::ast::BuiltInKeyword;

/// Describes the grammar of one YANG extension keyword.
#[derive(Debug, Clone, Copy)]
pub struct ExtensionGrammar {
    /// Resolved module name that defines the extension (e.g. `"acme-ext"`).
    pub module: &'static str,
    /// Local extension name (e.g. `"callpoint"`).
    pub name: &'static str,
    /// Built-in parent statement kinds in which this extension is valid.
    /// An empty slice means the extension is valid in any built-in context.
    pub parents: &'static [BuiltInKeyword],
    /// Whether the extension takes an argument and of what type.
    /// `None` means no argument is accepted.
    pub arg: Option<ArgType>,
    /// Sub-statements allowed inside an instance of this extension.
    pub substmts: &'static [SubstmtSpec],
}

/// Type of the extension's argument value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgType {
    /// A plain string (most common).
    String,
    /// A YANG identifier (`[a-zA-Z_][a-zA-Z0-9_\-\.]*`).
    YangIdentifier,
    /// An integer.
    Integer,
    /// Accept any non-empty argument without further validation.
    Any,
}

/// Specification for one allowed sub-statement of an extension instance.
#[derive(Debug, Clone, Copy)]
pub struct SubstmtSpec {
    /// The keyword expected.
    pub keyword: GrammarKeyword,
    /// How many times this sub-statement may appear.
    pub cardinality: Cardinality,
}

/// A keyword used inside a grammar rule — either a YANG built-in or another extension.
#[derive(Debug, Clone, Copy)]
pub enum GrammarKeyword {
    BuiltIn(BuiltInKeyword),
    Extension {
        module: &'static str,
        name: &'static str,
    },
    /// Wildcard: matches any extension sub-statement (any `prefix:local` keyword),
    /// regardless of which module or local name it has.
    /// Useful for extension containers like `acme:annotate` that are documented
    /// as accepting any sub-statement.
    AnyExtension,
    /// Wildcard: matches any YANG built-in sub-statement keyword.
    AnyBuiltIn,
}

/// Cardinality of a sub-statement within an extension grammar rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    /// Zero or one occurrences.
    Optional,
    /// Exactly one occurrence.
    Required,
    /// Any number of occurrences (including zero).
    ZeroOrMore,
    /// At least one occurrence.
    OneOrMore,
}

/// Registry of all extension grammar rules collected from plugins at startup.
///
/// Built once (before compilation begins) and stored in [`ModuleRegistry`].
/// The registry is shared read-only across all compilation threads.
#[derive(Default)]
pub struct GrammarRegistry {
    // Nested map: module_name → extension_name → rule.
    // String keys allow lookup with &str without lifetime friction.
    rules: HashMap<String, HashMap<String, &'static ExtensionGrammar>>,
}

impl GrammarRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register all grammar rules from one plugin's static declaration slice.
    pub fn register(&mut self, rules: &'static [ExtensionGrammar]) {
        for rule in rules {
            self.rules
                .entry(rule.module.to_string())
                .or_default()
                .insert(rule.name.to_string(), rule);
        }
    }

    /// Look up the grammar rule for a given `(module, extension_name)` pair.
    pub fn get(&self, module: &str, name: &str) -> Option<&'static ExtensionGrammar> {
        self.rules.get(module)?.get(name).copied()
    }

    /// Returns `true` when no grammar rules have been registered.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}
