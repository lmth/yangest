//! Tailf YANG extension grammar and annotation-overlay plugin for yangest.
//!
//! This plugin registers:
//!
//! 1. **Extension grammar** for all statements defined in `tailf-common.yang`,
//!    `tailf-meta-extensions.yang`, and `tailf-cli-extensions.yang`.
//!    Registering the grammar tells yangest's compiler to collect
//!    `ExtensionInstance` values on every `SchemaNode` and `CompiledModule`
//!    where these statements appear — without that, the compiler would emit
//!    unknown-keyword warnings and silently drop the extension data.
//!
//! 2. **Overlay extension** for `tailf:annotate`.  This tells the `ANNINDEX`
//!    phase to treat any `tailf:annotate "/path" { ... }` statement in an
//!    `--annotation-module` file as a per-node annotation: the body
//!    sub-statements are injected into the target node's `extensions` list
//!    at expansion time.
//!
//! The plugin itself produces no output (`-f tailf` is a no-op emitter).
//! Its value is entirely in the grammar/overlay registration, which applies
//! to every format selected via `-f`.  Use `-f yang-expanded` (or `-f tree`)
//! together with this plugin to see the full tailf-annotated schema.
//!
//! Sub-statement specifications are derived from the
//! `tailf-meta-extensions:substatement` annotations in the source YANG files.
//! Occurrence codes: no block = Optional, `"?"` = Optional, `"*"` = ZeroOrMore,
//! `"+"` = OneOrMore, `"1"` = Required.

use yangest_core::ast::BuiltInKeyword;
use yangest_core::grammar::{ArgType, Cardinality, ExtensionGrammar, GrammarKeyword, SubstmtSpec};
use yangest_core::plugin::{
    AstOverlayDescriptor, ExtensionId, OverlayExtension, Plugin, PluginRegistration,
};

// ── Plugin struct ─────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct TailfPlugin;

inventory::submit! {
    PluginRegistration { factory: || Box::new(TailfPlugin::default()) }
}

impl Plugin for TailfPlugin {
    fn name(&self) -> &'static str {
        "tailf"
    }

    fn yang_grammar(&self) -> &'static [ExtensionGrammar] {
        TAILF_GRAMMAR
    }

    fn overlay_extensions(&self) -> &'static [OverlayExtension] {
        TAILF_OVERLAY_EXTS
    }

    fn ast_overlay_extensions(&self) -> &'static [AstOverlayDescriptor] {
        TAILF_AST_OVERLAYS
    }
}

// ── Overlay extensions ────────────────────────────────────────────────────────

// `tailf:annotate-module` takes a module-name identifier as its argument (not a
// schema path), so it is registered via AstOverlayDescriptor rather than
// OverlayExtension.  `annotate-statement` is its paired statement-selector.
static TAILF_AST_OVERLAYS: &[AstOverlayDescriptor] = &[AstOverlayDescriptor {
    module_selector: ExtensionId {
        module: "tailf-common",
        name: "annotate-module",
    },
    stmt_selector: ExtensionId {
        module: "tailf-common",
        name: "annotate-statement",
    },
}];

static TAILF_OVERLAY_EXTS: &[OverlayExtension] = &[OverlayExtension {
    module: "tailf-common",
    name: "annotate",
    source_plugin: "tailf",
}];

// ── Sub-statement spec helpers ────────────────────────────────────────────────

const fn bi(kw: BuiltInKeyword) -> GrammarKeyword {
    GrammarKeyword::BuiltIn(kw)
}

const fn tc(name: &'static str) -> GrammarKeyword {
    GrammarKeyword::Extension {
        module: "tailf-common",
        name,
    }
}

const fn tce(name: &'static str) -> GrammarKeyword {
    GrammarKeyword::Extension {
        module: "tailf-cli-extensions",
        name,
    }
}

const fn opt(keyword: GrammarKeyword) -> SubstmtSpec {
    SubstmtSpec {
        keyword,
        cardinality: Cardinality::Optional,
    }
}

const fn many(keyword: GrammarKeyword) -> SubstmtSpec {
    SubstmtSpec {
        keyword,
        cardinality: Cardinality::ZeroOrMore,
    }
}

const fn one_plus(keyword: GrammarKeyword) -> SubstmtSpec {
    SubstmtSpec {
        keyword,
        cardinality: Cardinality::OneOrMore,
    }
}

const fn req(keyword: GrammarKeyword) -> SubstmtSpec {
    SubstmtSpec {
        keyword,
        cardinality: Cardinality::Required,
    }
}

/// Zero-or-more wildcard: accepts any extension sub-statement.
const fn any_ext() -> SubstmtSpec {
    SubstmtSpec {
        keyword: GrammarKeyword::AnyExtension,
        cardinality: Cardinality::ZeroOrMore,
    }
}

/// Zero-or-more wildcard: accepts any built-in sub-statement.
const fn any_builtin() -> SubstmtSpec {
    SubstmtSpec {
        keyword: GrammarKeyword::AnyBuiltIn,
        cardinality: Cardinality::ZeroOrMore,
    }
}

// ── Sub-statement specs for tailf-common.yang extensions ─────────────────────
//
// Derived from tailf-meta-extensions:substatement annotations in tailf-common.yang.
// No block / "?" = Optional, "*" = ZeroOrMore, "+" = OneOrMore, "1" = Required.

// tailf:annotate and tailf:annotate-statement accept "any tailf statement, except
// tailf:action" per the tailf-common.yang description, plus YANG built-in validation
// statements (must, when, mandatory, min-elements, max-elements, unique).
// Use wildcard specs rather than exhaustively listing every tailf extension.
static ANNOTATE_SUBSTMTS: &[SubstmtSpec] = &[any_ext(), any_builtin()];

static ANNOTATE_STATEMENT_SUBSTMTS: &[SubstmtSpec] = &[any_ext(), any_builtin()];

static ANNOTATE_MODULE_SUBSTMTS: &[SubstmtSpec] = &[
    opt(tc("internal-dp")),
    opt(tc("snmp-oid")),
    opt(tc("snmp-mib-module-name")),
    opt(tc("id")),
    opt(tc("id-value")),
    many(tc("export")),
    opt(tc("unique-selector")),
    many(tc("annotate-statement")),
];

static VALUE_LENGTH_SUBSTMTS: &[SubstmtSpec] = &[
    opt(bi(BuiltInKeyword::ErrorMessage)),
    opt(bi(BuiltInKeyword::ErrorAppTag)),
];

static PATH_FILTERS_SUBSTMTS: &[SubstmtSpec] = &[opt(tc("no-subtree-match"))];

static CALLPOINT_SUBSTMTS: &[SubstmtSpec] = &[
    opt(bi(BuiltInKeyword::Description)),
    opt(tc("config")),
    opt(tc("transform")),
    opt(tc("set-hook")),
    opt(tc("transaction-hook")),
    opt(tc("cache")),
    opt(tc("opaque")),
    opt(tc("operational")),
    opt(tc("internal")),
];

static TRANSACTION_HOOK_SUBSTMTS: &[SubstmtSpec] = &[opt(tc("invocation-mode"))];

static CACHE_SUBSTMTS: &[SubstmtSpec] = &[opt(tc("timeout"))];

static CDB_OPER_SUBSTMTS: &[SubstmtSpec] = &[
    opt(bi(BuiltInKeyword::Description)),
    opt(tc("operational")),
    opt(tc("persistent")),
];

static LINK_SUBSTMTS: &[SubstmtSpec] = &[opt(tc("inherit-set-hook"))];

static SECONDARY_INDEX_SUBSTMTS: &[SubstmtSpec] = &[
    req(tc("index-leafs")),
    opt(tc("sort-order")),
    opt(tc("display-default-order")),
];

static UNIQUE_SELECTOR_SUBSTMTS: &[SubstmtSpec] = &[one_plus(tc("unique-leaf"))];

static VALIDATE_SUBSTMTS: &[SubstmtSpec] = &[
    opt(bi(BuiltInKeyword::Description)),
    opt(tc("call-once")),
    many(tc("dependency")),
    opt(tc("no-dependency")),
    opt(tc("opaque")),
    opt(tc("internal")),
    opt(tc("priority")),
];

static DEPENDENCY_SUBSTMTS: &[SubstmtSpec] = &[opt(tc("xpath-root"))];

static DISPLAY_WHEN_SUBSTMTS: &[SubstmtSpec] = &[opt(tc("xpath-root"))];

static SNMP_DELETE_VALUE_SUBSTMTS: &[SubstmtSpec] = &[opt(tc("snmp-send-delete-value"))];

static ACTION_SUBSTMTS: &[SubstmtSpec] = &[
    opt(bi(BuiltInKeyword::Description)),
    many(bi(BuiltInKeyword::Grouping)),
    many(bi(BuiltInKeyword::IfFeature)),
    opt(bi(BuiltInKeyword::Reference)),
    opt(bi(BuiltInKeyword::Input)),
    opt(bi(BuiltInKeyword::Output)),
    opt(bi(BuiltInKeyword::Status)),
    many(bi(BuiltInKeyword::Typedef)),
    opt(tc("actionpoint")),
    opt(tc("alt-name")),
    opt(tce("cli-mount-point")),
    opt(tce("cli-configure-mode")),
    opt(tce("cli-operational-mode")),
    opt(tce("cli-oper-info")),
    opt(tc("code-name")),
    opt(tc("confirm-text")),
    opt(tc("display-when")),
    opt(tc("exec")),
    many(tc("hidden")),
    opt(tc("info")),
    opt(tc("info-html")),
];

static ACTIONPOINT_SUBSTMTS: &[SubstmtSpec] = &[opt(tc("opaque")), opt(tc("internal"))];

static CONFIRM_TEXT_SUBSTMTS: &[SubstmtSpec] = &[
    opt(tc("confirm-default")),
    opt(tce("cli-batch-confirm-default")),
];

static INDEXED_VIEW_SUBSTMTS: &[SubstmtSpec] = &[opt(tc("auto-compact"))];

static ERROR_INFO_SUBSTMTS: &[SubstmtSpec] = &[
    opt(bi(BuiltInKeyword::Description)),
    many(bi(BuiltInKeyword::Leaf)),
    many(bi(BuiltInKeyword::LeafList)),
    many(bi(BuiltInKeyword::List)),
    many(bi(BuiltInKeyword::Container)),
    many(bi(BuiltInKeyword::Choice)),
    many(bi(BuiltInKeyword::Uses)),
];

static NON_STRICT_LEAFREF_SUBSTMTS: &[SubstmtSpec] = &[req(bi(BuiltInKeyword::Path))];

static EXEC_SUBSTMTS: &[SubstmtSpec] = &[
    opt(tc("args")),
    opt(tc("uid")),
    opt(tc("gid")),
    opt(tc("wd")),
    opt(tc("global-no-duplicate")),
    opt(tc("raw-xml")),
    opt(tc("interruptible")),
    opt(tc("interrupt")),
];

static RAW_XML_SUBSTMTS: &[SubstmtSpec] = &[opt(tc("batch"))];

static INTERNAL_SUBSTMTS: &[SubstmtSpec] = &[opt(tce("cli-commit-prompt"))];

static STRUCTURE_SUBSTMTS: &[SubstmtSpec] = &[
    opt(bi(BuiltInKeyword::Description)),
    many(bi(BuiltInKeyword::Leaf)),
    many(bi(BuiltInKeyword::LeafList)),
    many(bi(BuiltInKeyword::List)),
    many(bi(BuiltInKeyword::Container)),
    many(bi(BuiltInKeyword::Choice)),
    many(bi(BuiltInKeyword::Uses)),
];

static META_DATA_SUBSTMTS: &[SubstmtSpec] = &[opt(tc("meta-value"))];

static NED_DATA_SUBSTMTS: &[SubstmtSpec] = &[
    opt(tc("transaction")),
    opt(tc("xpath-root")),
    many(tc("operation")),
];

static MOUNT_POINT_SUBSTMTS: &[SubstmtSpec] = &[opt(tc("mount-id"))];

// ── Extension grammar — tailf-common.yang ────────────────────────────────────
//
// All entries use `parents: &[]` (valid in any context).  Extensions with
// declared sub-statements carry proper SubstmtSpec slices; all others use
// `substmts: &[]` (no sub-statements, which is correct per tailf-common.yang).

static TAILF_GRAMMAR: &[ExtensionGrammar] = &[
    // tailf-common.yang
    ExtensionGrammar {
        module: "tailf-common",
        name: "abstract",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "action",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: ACTION_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "actionpoint",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: ACTIONPOINT_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "alt-name",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "annotate",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: ANNOTATE_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "annotate-module",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: ANNOTATE_MODULE_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "annotate-statement",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: ANNOTATE_STATEMENT_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "args",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "auto-compact",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "batch",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "cache",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: CACHE_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "call-once",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "callpoint",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: CALLPOINT_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "cdb-oper",
        parents: &[],
        arg: None,
        substmts: CDB_OPER_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "code-name",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "config",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "confirm-default",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "confirm-text",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: CONFIRM_TEXT_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "default-ref",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "dependency",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: DEPENDENCY_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "display-column-name",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "display-default-order",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "display-groups",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "display-hint",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "display-status-name",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "display-when",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: DISPLAY_WHEN_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "error-info",
        parents: &[],
        arg: None,
        substmts: ERROR_INFO_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "exec",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: EXEC_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "export",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "gid",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "global-no-duplicate",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "hidden",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "id",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "id-value",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "ignore-if-no-cdb-oper",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "index-leafs",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "indexed-view",
        parents: &[],
        arg: None,
        substmts: INDEXED_VIEW_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "info",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "info-html",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "inherit-set-hook",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "internal",
        parents: &[],
        arg: None,
        substmts: INTERNAL_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "internal-dp",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "interrupt",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "interruptible",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "invocation-mode",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "java-class-name",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "junos-val-as-xml-tag",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "junos-val-with-prev-xml-tag",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "key-default",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "link",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: LINK_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "lower-case",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "meta-data",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: META_DATA_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "meta-value",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "mount-id",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "mount-point",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: MOUNT_POINT_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "ncs-device-type",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "ned-data",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: NED_DATA_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "ned-default-handling",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "ned-ignore-compare-config",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "no-dependency",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "no-leafref-check",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "no-subtree-match",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "non-strict-leafref",
        parents: &[],
        arg: None,
        substmts: NON_STRICT_LEAFREF_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "opaque",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "operation",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "operational",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "override-auto-dependencies",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "path-filters",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: PATH_FILTERS_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "persistent",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "priority",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "prompt",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "raw-xml",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: RAW_XML_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "secondary-index",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: SECONDARY_INDEX_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "set-hook",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "snmp-delete-value",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: SNMP_DELETE_VALUE_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "snmp-exclude-object",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "snmp-lax-type-check",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "snmp-mib-module-name",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "snmp-name",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "snmp-ned-accessible-column",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "snmp-ned-delete-before-create",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "snmp-ned-modification-dependent",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "snmp-ned-recreate-when-modified",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "snmp-ned-set-before-row-modification",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "snmp-oid",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "snmp-row-status-column",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "snmp-send-delete-value",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "sort-order",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "sort-priority",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "step",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "structure",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: STRUCTURE_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "suppress-echo",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "timeout",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "transaction",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "transaction-hook",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: TRANSACTION_HOOK_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "transform",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "typepoint",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "uid",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "unique-leaf",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "unique-selector",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: UNIQUE_SELECTOR_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "validate",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: VALIDATE_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "value-length",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: VALUE_LENGTH_SUBSTMTS,
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "wd",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "writable",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-common",
        name: "xpath-root",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    // tailf-meta-extensions.yang
    ExtensionGrammar {
        module: "tailf-meta-extensions",
        name: "arg-type",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-meta-extensions",
        name: "occurence",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-meta-extensions",
        name: "substatement",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-meta-extensions",
        name: "use-in",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    // tailf-cli-extensions.yang — all registered with substmts: &[] since their own
    // sub-statement relationships are internal to the CLI engine and not required for
    // schema analysis.
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-allow-caching",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-allow-join-with-key",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-allow-join-with-value",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-allow-key-abbreviation",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-allow-range",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-allow-wildcard",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-auto-legend",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-autowizard",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-batch-confirm-default",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-before-key",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-boolean-no",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-break-sequence-commands",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-case-insensitive",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-case-sensitive",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-column-align",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-column-stats",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-column-width",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-commit-prompt",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-compact-stats",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-compact-syntax",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-completion-actionpoint",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-completion-id",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-configure-mode",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-custom-error",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-custom-range",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-custom-range-actionpoint",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-custom-range-enumerator",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-delayed-auto-commit",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-delete-container-on-delete",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-delete-when-empty",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-delimiter",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-diff-after",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-diff-before",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-diff-create-after",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-diff-create-before",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-diff-delete-after",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-diff-delete-before",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-diff-dependency",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-diff-modify-after",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-diff-modify-before",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-diff-set-after",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-diff-set-before",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-disabled-info",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-disallow-value",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-display-empty-config",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-display-joined",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-display-separated",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-drop-node-name",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-embed-no-on-delete",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-enforce-table",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-exit-command",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-explicit-exit",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-expose-key-name",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-expose-ns-prefix",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-flat-list-syntax",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-flatten-container",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-full-command",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-full-no",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-full-show-path",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-hide-in-submode",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-ignore-modified",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-incomplete-command",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-incomplete-no",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-incomplete-show-path",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-instance-info-leafs",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-key-format",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-list-syntax",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-max-keys",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-max-words",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-min-column-width",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-min-keys",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-mode-name",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-mode-name-actionpoint",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-mount-point",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-multi-line-prompt",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-multi-value",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-multi-word",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-multi-word-key",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-no-key-completion",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-no-keyword",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-no-match-completion",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-no-name-on-delete",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-no-value-on-delete",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-oper-info",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-only-in-autowizard",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-operational-mode",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-optional-in-sequence",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-prefix-key",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-preformatted",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-prettify",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-range-delimiters",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-range-list-syntax",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-range-type",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-recursive-delete",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-remove-before-change",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-replace-all",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-reset-all-siblings",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-reset-container",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-reset-full",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-reset-siblings",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-reversed",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-run-template",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-run-template-enter",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-run-template-footer",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-run-template-legend",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-sequence-commands",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-short-no",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-show-config",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-show-long-obu-diffs",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-show-no",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-show-obu-comments",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-show-order-tag",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-show-order-taglist",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-show-template",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-show-template-enter",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-show-template-footer",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-show-template-legend",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-show-with-default",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-spacer",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-strict-leafref",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-error-message-value",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-key-abbreviation",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-key-sort",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-leafref-in-diff",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-list-no",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-mode",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-no",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-quotes",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-range",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-shortenabled",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-show-conf-path",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-show-match",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-show-path",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-silent-no",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-table",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-validation-warning-prompt",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-warning",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-suppress-wildcard",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-table-footer",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-table-legend",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-trim-default",
        parents: &[],
        arg: None,
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-trigger-on-all",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-trigger-on-delete",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-trigger-on-set",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-value-display-template",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-when-target-create",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-when-target-delete",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-when-target-modify",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-when-target-set",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-width",
        parents: &[],
        arg: Some(ArgType::Any),
        substmts: &[],
    },
    ExtensionGrammar {
        module: "tailf-cli-extensions",
        name: "cli-wrap",
        parents: &[],
        arg: None,
        substmts: &[],
    },
];
