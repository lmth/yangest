//! yangest FXS plugin — produces `.fxs` files for ConfD/NSO.
//!
//! Register this plugin by linking it into the yangest binary:
//! ```
//! use yangest_plugin_fxs as _;
//! ```

pub mod emit;
#[cfg(feature = "yanger-compat-hash-order")]
pub mod genie;
pub mod hash;
pub mod header;
pub mod schema;
pub mod serial;
pub mod terms;
pub mod thash;
pub mod types;
pub mod xpath_compiler;

use std::io::Write;
use std::sync::Arc;

use clap::{Arg, ArgAction, ArgMatches};
use yangest_core::compiler::{CompiledModule, ExpansionCtx, ModuleRegistry};
use yangest_core::plugin::{BundleState, EmitState, Plugin, PluginRegistration};

/// FXS output plugin.
pub struct FxsPlugin {
    no_yang_source: bool,
}

impl Default for FxsPlugin {
    fn default() -> Self {
        FxsPlugin { no_yang_source: false }
    }
}

inventory::submit! {
    PluginRegistration { factory: || Box::new(FxsPlugin::default()) }
}

impl Plugin for FxsPlugin {
    fn name(&self) -> &'static str {
        "fxs"
    }

    fn extension(&self) -> &'static str {
        "fxs"
    }

    fn cli_args(&self) -> Vec<Arg> {
        vec![
            Arg::new("fxs-no-yang-source")
                .long("fxs-no-yang-source")
                .action(ArgAction::SetTrue)
                .help("Do not include the YANG module text in the fxs file"),
        ]
    }

    fn configure(&mut self, matches: &ArgMatches) {
        self.no_yang_source = matches.get_flag("fxs-no-yang-source");
    }

    fn emit_module(
        &self,
        module: &Arc<CompiledModule>,
        registry: &ModuleRegistry,
        ctx: &ExpansionCtx<'_>,
        _bundle: &BundleState,
        _state: &mut EmitState,
        out: &mut dyn Write,
    ) -> std::io::Result<()> {
        emit::emit_fxs(module, registry, ctx, out, self.no_yang_source)
    }
}
