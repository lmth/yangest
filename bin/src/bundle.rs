// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Bundle file loading and path resolution.
//!
//! A `.yangbundle` file (TOML) is the authoritative, self-contained
//! description of what to compile: which modules are primary, where to find
//! dependencies, which features are enabled, and which deviation modules to
//! apply.  It carries no output-format concerns — those stay on the CLI.
//!
//! All paths in the bundle file are resolved relative to the bundle file's
//! own directory, so bundles are portable regardless of the working directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Parsed representation of a `.yangbundle` TOML file.
#[derive(Debug, Deserialize)]
pub struct BundleFile {
    /// Primary modules to compile and emit output for.
    pub modules: Vec<PathBuf>,

    /// Directories added to the YANG module search path.
    #[serde(default)]
    pub search_paths: Vec<PathBuf>,

    /// Deviation modules — applied to their target modules but not emitted.
    #[serde(default)]
    pub deviation_modules: Vec<PathBuf>,

    /// Annotation modules — overlay files that attach plugin-declared
    /// extension annotations to target nodes.  Applied at expansion time but
    /// not emitted as output.
    #[serde(default)]
    pub annotation_modules: Vec<PathBuf>,

    /// Feature enablement: module name → list of enabled feature names.
    ///
    /// ```toml
    /// [features]
    /// "ietf-interfaces" = ["arbitrary-names", "pre-provisioning"]
    /// ```
    #[serde(default)]
    pub features: HashMap<String, Vec<String>>,

    /// Global (module-unqualified) feature names to enable in every module that declares them.
    ///
    /// ```toml
    /// global-features = ["my-feature"]
    /// ```
    #[serde(rename = "global-features", default)]
    pub global_features: Vec<String>,
}

impl BundleFile {
    /// Load and parse a bundle file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read bundle '{}': {}", path.display(), e))?;
        toml::from_str(&src)
            .map_err(|e| format!("invalid bundle '{}': {}", path.display(), e))
    }

    /// Resolve all relative paths in the bundle against `base` (the bundle
    /// file's parent directory) so they can be used from any working directory.
    pub fn resolve_paths(&mut self, base: &Path) {
        for p in &mut self.modules {
            *p = resolve(base, p);
        }
        for p in &mut self.search_paths {
            *p = resolve(base, p);
        }
        for p in &mut self.deviation_modules {
            *p = resolve(base, p);
        }
        for p in &mut self.annotation_modules {
            *p = resolve(base, p);
        }
    }
}

fn resolve(base: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}
