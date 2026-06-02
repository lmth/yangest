// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Type-resolution observer (feature #7: incremental namespace registration).
//!
//! The reference compiler builds its `ns_to_prefix_maps` incrementally: every
//! time it resolves a leafref or a derived type it records the source module, so
//! the final list is in *encounter order during type resolution*, not in any
//! sorted order. No pure post-pass sort reproduces this for all modules, so
//! yangest exposes the encounter stream directly.
//!
//! A backend supplies an observer to
//! [`TypeRegistry::resolve_with_observer`](super::TypeRegistry::resolve_with_observer);
//! it fires once per resolved typedef/leafref, in the order the backend drives
//! resolution. The plain `resolve` path passes a [`NoopObserver`], whose empty
//! methods the compiler inlines away — so non-observers pay nothing.
//!
//! ## Firing semantics
//!
//! * [`on_leafref_resolved`](TypeResolutionObserver::on_leafref_resolved) fires
//!   in exact encounter order: leafref resolution is never memoized, so every
//!   leafref the backend resolves reports its target. This is the signal the
//!   reference compiler uses to build `ns_to_prefix_maps`, where no post-pass
//!   sort reproduces the order — so encounter order is exactly what is exposed.
//! * [`on_typedef_resolved`](TypeResolutionObserver::on_typedef_resolved) fires
//!   for the *referenced* typedef on every encounter (even a cached one). The
//!   inner derivation chain of an already-resolved typedef is not re-walked, so
//!   a chain's nested typedefs report on first resolution; thereafter only the
//!   outer reference reports. Drive resolution in the order you need the
//!   namespaces registered.

use crate::compiler::{SchemaNode, Typedef};

/// Receives type-resolution events in encounter order.
pub trait TypeResolutionObserver {
    /// Fired when a typedef reference is resolved. `source_module` is the module
    /// that *defines* the typedef.
    fn on_typedef_resolved(&mut self, _source_module: &str, _typedef: &Typedef) {}

    /// Fired when a leafref's target node is resolved. `source_module` is the
    /// module in whose context the leafref was resolved.
    fn on_leafref_resolved(&mut self, _source_module: &str, _target: &SchemaNode) {}
}

/// An observer that records nothing. Used by the non-observer resolution paths;
/// its empty methods are inlined away, so it imposes no cost.
pub struct NoopObserver;

impl TypeResolutionObserver for NoopObserver {}
