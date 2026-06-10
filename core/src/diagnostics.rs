// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Plugin-facing diagnostics: a thread-safe channel for plugins to raise
//! warnings and errors during emission.
//!
//! Compile/parse diagnostics use [`crate::ast::YError`]; that type is closed
//! (its [`crate::ast::ErrorCode`] is a fixed enum) and oriented at the compiler.
//! Plugins are open-ended, so they raise the simpler [`Diagnostic`] here, with a
//! free-form string `code` for optional `-Werror=<code>`-style selection.
//!
//! The collector is shared by `&` across the parallel per-module emit path (each
//! worker builds its own [`ExpansionCtx`](crate::compiler::ExpansionCtx) but they
//! all point at the same [`Diagnostics`]), so it is internally synchronised.

use std::fmt;
use std::sync::Mutex;

pub use crate::ast::Level;

/// A single diagnostic raised by a plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: Level,
    /// The plugin that raised it (its [`name`](crate::plugin::Plugin::name)).
    pub plugin: String,
    /// Optional symbolic code, for `-Werror=<code>` / suppression by a host.
    pub code: Option<String>,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.code {
            Some(code) => write!(f, "{}: {}: [{}] {}", self.plugin, self.level, code, self.message),
            None => write!(f, "{}: {}: {}", self.plugin, self.level, self.message),
        }
    }
}

/// A thread-safe sink that plugins push diagnostics into during emit.
///
/// Obtain one from the emit context via
/// [`ExpansionCtx::diagnostics`](crate::compiler::ExpansionCtx::diagnostics);
/// the host (the `yangest` binary) creates it, wires it into the context, and
/// drains it after emission to decide the exit status (honouring `--werror`).
#[derive(Default)]
pub struct Diagnostics {
    items: Mutex<Vec<Diagnostic>>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Raise a warning.
    pub fn warning(&self, plugin: impl Into<String>, message: impl Into<String>) {
        self.push(Level::Warning, plugin, None, message);
    }

    /// Raise a warning carrying a symbolic code (for `-Werror=<code>` selection).
    pub fn warning_with_code(
        &self,
        plugin: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.push(Level::Warning, plugin, Some(code.into()), message);
    }

    /// Raise an error (always fatal, regardless of `--werror`).
    pub fn error(&self, plugin: impl Into<String>, message: impl Into<String>) {
        self.push(Level::Error, plugin, None, message);
    }

    /// Raise an error carrying a symbolic code.
    pub fn error_with_code(
        &self,
        plugin: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.push(Level::Error, plugin, Some(code.into()), message);
    }

    fn push(
        &self,
        level: Level,
        plugin: impl Into<String>,
        code: Option<String>,
        message: impl Into<String>,
    ) {
        self.items.lock().unwrap().push(Diagnostic {
            level,
            plugin: plugin.into(),
            code,
            message: message.into(),
        });
    }

    /// True if any diagnostic has been raised.
    pub fn is_empty(&self) -> bool {
        self.items.lock().unwrap().is_empty()
    }

    /// Drain all collected diagnostics (in the order raised).
    pub fn take(&self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.items.lock().unwrap())
    }
}

/// Whether a diagnostic level is fatal given the host's `--werror` setting:
/// errors are always fatal; warnings are fatal only under `werror`.
pub fn is_fatal(level: Level, werror: bool) -> bool {
    matches!(level, Level::Error) || (werror && matches!(level, Level::Warning))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_and_drains() {
        let d = Diagnostics::new();
        assert!(d.is_empty());
        d.warning("p", "a warning");
        d.error_with_code("p", "E1", "an error");
        assert!(!d.is_empty());

        let items = d.take();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].level, Level::Warning);
        assert_eq!(items[1].level, Level::Error);
        assert_eq!(items[1].code.as_deref(), Some("E1"));
        assert!(d.is_empty(), "take() drains");
        assert_eq!(items[0].to_string(), "p: warning: a warning");
        assert_eq!(items[1].to_string(), "p: error: [E1] an error");
    }

    #[test]
    fn fatality_respects_werror() {
        assert!(is_fatal(Level::Error, false));
        assert!(is_fatal(Level::Error, true));
        assert!(!is_fatal(Level::Warning, false));
        assert!(is_fatal(Level::Warning, true));
    }
}
