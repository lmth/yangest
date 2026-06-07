// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Core YANG AST types shared across all pipeline stages.

use std::fmt;
use std::sync::Arc;

// ── Source positions ──────────────────────────────────────────────────────────

/// Source location of a statement or argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pos {
    /// Direct source location.
    FilePos {
        file: Arc<str>,
        line: u32,
    },
    /// Location of a node instantiated from a grouping via `uses`.
    /// Carries both the `uses` statement position and the original definition position.
    UsesPos {
        uses_pos: Box<Pos>,
        orig_pos: Box<Pos>,
    },
}

impl Pos {
    pub fn new(file: Arc<str>, line: u32) -> Self {
        Pos::FilePos { file, line }
    }

    pub fn file(&self) -> &Arc<str> {
        match self {
            Pos::FilePos { file, .. } => file,
            Pos::UsesPos { uses_pos, .. } => uses_pos.file(),
        }
    }

    /// Returns the file of the original definition site, ignoring any `uses`
    /// context.  For a `FilePos` this is the same as [`file()`].  For a
    /// `UsesPos` this recurses into `orig_pos` instead of `uses_pos`, giving
    /// the file where the statement was originally defined (e.g. a grouping
    /// body or an annotation module), not where it was instantiated.
    pub fn orig_file(&self) -> &Arc<str> {
        match self {
            Pos::FilePos { file, .. } => file,
            Pos::UsesPos { orig_pos, .. } => orig_pos.orig_file(),
        }
    }

    /// Returns the line number of the original definition site, ignoring any
    /// `uses` context.  Mirrors [`orig_file()`] for the line component.
    pub fn orig_line(&self) -> u32 {
        match self {
            Pos::FilePos { line, .. } => *line,
            Pos::UsesPos { orig_pos, .. } => orig_pos.orig_line(),
        }
    }

    pub fn line(&self) -> u32 {
        match self {
            Pos::FilePos { line, .. } => *line,
            Pos::UsesPos { uses_pos, .. } => uses_pos.line(),
        }
    }
}

impl fmt::Display for Pos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pos::FilePos { file, line } => write!(f, "{}:{}", file, line),
            Pos::UsesPos { uses_pos, .. } => write!(f, "{} (via uses)", uses_pos),
        }
    }
}

// ── Keywords ──────────────────────────────────────────────────────────────────

/// All built-in YANG keywords (YANG 1.0 + 1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltInKeyword {
    // Module / submodule header
    Module,
    Submodule,
    YangVersion,
    Namespace,
    Prefix,
    Import,
    Include,
    RevisionDate,
    BelongsTo,
    // Meta
    Organization,
    Contact,
    Description,
    Reference,
    // Revision
    Revision,
    // Extension
    Extension,
    Argument,
    YinElement,
    // Features & identities
    Feature,
    IfFeature,
    Identity,
    Base,
    // Typedefs
    Typedef,
    Type,
    Units,
    Default,
    // Groupings
    Grouping,
    Uses,
    Refine,
    Augment,
    // Data nodes
    Container,
    Presence,
    Leaf,
    LeafList,
    List,
    Key,
    Unique,
    Choice,
    Case,
    AnyXml,
    AnyData, // YANG 1.1
    // Operation nodes
    Rpc,
    Action, // YANG 1.1
    Input,
    Output,
    Notification,
    // Type constraints
    Range,
    Length,
    Pattern,
    Modifier, // YANG 1.1
    EnumStmt,
    Bit,
    Position,
    Value,
    FractionDigits,
    Path,
    RequireInstance,
    // Schema properties
    Config,
    Mandatory,
    MinElements,
    MaxElements,
    OrderedBy,
    Status,
    When,
    Must,
    ErrorMessage,
    ErrorAppTag,
    // Deviation
    Deviation,
    Deviate,
}

impl BuiltInKeyword {
    /// Returns the YANG keyword string, e.g. `"leaf-list"`.
    pub fn as_str(self) -> &'static str {
        match self {
            BuiltInKeyword::Module => "module",
            BuiltInKeyword::Submodule => "submodule",
            BuiltInKeyword::YangVersion => "yang-version",
            BuiltInKeyword::Namespace => "namespace",
            BuiltInKeyword::Prefix => "prefix",
            BuiltInKeyword::Import => "import",
            BuiltInKeyword::Include => "include",
            BuiltInKeyword::RevisionDate => "revision-date",
            BuiltInKeyword::BelongsTo => "belongs-to",
            BuiltInKeyword::Organization => "organization",
            BuiltInKeyword::Contact => "contact",
            BuiltInKeyword::Description => "description",
            BuiltInKeyword::Reference => "reference",
            BuiltInKeyword::Revision => "revision",
            BuiltInKeyword::Extension => "extension",
            BuiltInKeyword::Argument => "argument",
            BuiltInKeyword::YinElement => "yin-element",
            BuiltInKeyword::Feature => "feature",
            BuiltInKeyword::IfFeature => "if-feature",
            BuiltInKeyword::Identity => "identity",
            BuiltInKeyword::Base => "base",
            BuiltInKeyword::Typedef => "typedef",
            BuiltInKeyword::Type => "type",
            BuiltInKeyword::Units => "units",
            BuiltInKeyword::Default => "default",
            BuiltInKeyword::Grouping => "grouping",
            BuiltInKeyword::Uses => "uses",
            BuiltInKeyword::Refine => "refine",
            BuiltInKeyword::Augment => "augment",
            BuiltInKeyword::Container => "container",
            BuiltInKeyword::Presence => "presence",
            BuiltInKeyword::Leaf => "leaf",
            BuiltInKeyword::LeafList => "leaf-list",
            BuiltInKeyword::List => "list",
            BuiltInKeyword::Key => "key",
            BuiltInKeyword::Unique => "unique",
            BuiltInKeyword::Choice => "choice",
            BuiltInKeyword::Case => "case",
            BuiltInKeyword::AnyXml => "anyxml",
            BuiltInKeyword::AnyData => "anydata",
            BuiltInKeyword::Rpc => "rpc",
            BuiltInKeyword::Action => "action",
            BuiltInKeyword::Input => "input",
            BuiltInKeyword::Output => "output",
            BuiltInKeyword::Notification => "notification",
            BuiltInKeyword::Range => "range",
            BuiltInKeyword::Length => "length",
            BuiltInKeyword::Pattern => "pattern",
            BuiltInKeyword::Modifier => "modifier",
            BuiltInKeyword::EnumStmt => "enum",
            BuiltInKeyword::Bit => "bit",
            BuiltInKeyword::Position => "position",
            BuiltInKeyword::Value => "value",
            BuiltInKeyword::FractionDigits => "fraction-digits",
            BuiltInKeyword::Path => "path",
            BuiltInKeyword::RequireInstance => "require-instance",
            BuiltInKeyword::Config => "config",
            BuiltInKeyword::Mandatory => "mandatory",
            BuiltInKeyword::MinElements => "min-elements",
            BuiltInKeyword::MaxElements => "max-elements",
            BuiltInKeyword::OrderedBy => "ordered-by",
            BuiltInKeyword::Status => "status",
            BuiltInKeyword::When => "when",
            BuiltInKeyword::Must => "must",
            BuiltInKeyword::ErrorMessage => "error-message",
            BuiltInKeyword::ErrorAppTag => "error-app-tag",
            BuiltInKeyword::Deviation => "deviation",
            BuiltInKeyword::Deviate => "deviate",
        }
    }

    /// Parse from string. Returns `None` if not a built-in keyword.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "module" => Some(BuiltInKeyword::Module),
            "submodule" => Some(BuiltInKeyword::Submodule),
            "yang-version" => Some(BuiltInKeyword::YangVersion),
            "namespace" => Some(BuiltInKeyword::Namespace),
            "prefix" => Some(BuiltInKeyword::Prefix),
            "import" => Some(BuiltInKeyword::Import),
            "include" => Some(BuiltInKeyword::Include),
            "revision-date" => Some(BuiltInKeyword::RevisionDate),
            "belongs-to" => Some(BuiltInKeyword::BelongsTo),
            "organization" => Some(BuiltInKeyword::Organization),
            "contact" => Some(BuiltInKeyword::Contact),
            "description" => Some(BuiltInKeyword::Description),
            "reference" => Some(BuiltInKeyword::Reference),
            "revision" => Some(BuiltInKeyword::Revision),
            "extension" => Some(BuiltInKeyword::Extension),
            "argument" => Some(BuiltInKeyword::Argument),
            "yin-element" => Some(BuiltInKeyword::YinElement),
            "feature" => Some(BuiltInKeyword::Feature),
            "if-feature" => Some(BuiltInKeyword::IfFeature),
            "identity" => Some(BuiltInKeyword::Identity),
            "base" => Some(BuiltInKeyword::Base),
            "typedef" => Some(BuiltInKeyword::Typedef),
            "type" => Some(BuiltInKeyword::Type),
            "units" => Some(BuiltInKeyword::Units),
            "default" => Some(BuiltInKeyword::Default),
            "grouping" => Some(BuiltInKeyword::Grouping),
            "uses" => Some(BuiltInKeyword::Uses),
            "refine" => Some(BuiltInKeyword::Refine),
            "augment" => Some(BuiltInKeyword::Augment),
            "container" => Some(BuiltInKeyword::Container),
            "presence" => Some(BuiltInKeyword::Presence),
            "leaf" => Some(BuiltInKeyword::Leaf),
            "leaf-list" => Some(BuiltInKeyword::LeafList),
            "list" => Some(BuiltInKeyword::List),
            "key" => Some(BuiltInKeyword::Key),
            "unique" => Some(BuiltInKeyword::Unique),
            "choice" => Some(BuiltInKeyword::Choice),
            "case" => Some(BuiltInKeyword::Case),
            "anyxml" => Some(BuiltInKeyword::AnyXml),
            "anydata" => Some(BuiltInKeyword::AnyData),
            "rpc" => Some(BuiltInKeyword::Rpc),
            "action" => Some(BuiltInKeyword::Action),
            "input" => Some(BuiltInKeyword::Input),
            "output" => Some(BuiltInKeyword::Output),
            "notification" => Some(BuiltInKeyword::Notification),
            "range" => Some(BuiltInKeyword::Range),
            "length" => Some(BuiltInKeyword::Length),
            "pattern" => Some(BuiltInKeyword::Pattern),
            "modifier" => Some(BuiltInKeyword::Modifier),
            "enum" => Some(BuiltInKeyword::EnumStmt),
            "bit" => Some(BuiltInKeyword::Bit),
            "position" => Some(BuiltInKeyword::Position),
            "value" => Some(BuiltInKeyword::Value),
            "fraction-digits" => Some(BuiltInKeyword::FractionDigits),
            "path" => Some(BuiltInKeyword::Path),
            "require-instance" => Some(BuiltInKeyword::RequireInstance),
            "config" => Some(BuiltInKeyword::Config),
            "mandatory" => Some(BuiltInKeyword::Mandatory),
            "min-elements" => Some(BuiltInKeyword::MinElements),
            "max-elements" => Some(BuiltInKeyword::MaxElements),
            "ordered-by" => Some(BuiltInKeyword::OrderedBy),
            "status" => Some(BuiltInKeyword::Status),
            "when" => Some(BuiltInKeyword::When),
            "must" => Some(BuiltInKeyword::Must),
            "error-message" => Some(BuiltInKeyword::ErrorMessage),
            "error-app-tag" => Some(BuiltInKeyword::ErrorAppTag),
            "deviation" => Some(BuiltInKeyword::Deviation),
            "deviate" => Some(BuiltInKeyword::Deviate),
            _ => None,
        }
    }
}

impl fmt::Display for BuiltInKeyword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A YANG keyword — either built-in or an extension.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Keyword {
    BuiltIn(BuiltInKeyword),
    /// Extension keyword `prefix:local` resolved to `(module_name, local_name)`.
    Extension { module: String, name: String },
    /// Extension keyword with unresolved prefix (pre-resolution stage).
    ExtensionPrefixed { prefix: String, name: String },
}

impl Keyword {
    pub fn is_builtin(&self, kw: BuiltInKeyword) -> bool {
        matches!(self, Keyword::BuiltIn(k) if *k == kw)
    }
}

impl fmt::Display for Keyword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Keyword::BuiltIn(kw) => write!(f, "{}", kw),
            Keyword::Extension { module, name } => write!(f, "{}:{}", module, name),
            Keyword::ExtensionPrefixed { prefix, name } => write!(f, "{}:{}", prefix, name),
        }
    }
}

// ── Raw AST statement ─────────────────────────────────────────────────────────

/// A raw parsed YANG statement, as produced by the parser before semantic analysis.
///
/// The argument is stored as a plain `String` at this stage. Typed parsing
/// (range-arg, path-arg, etc.) happens during compilation.
#[derive(Debug, Clone)]
pub struct Stmt {
    pub keyword: Keyword,
    /// The raw string argument, if present. Still unescaped/normalized.
    pub arg: Option<String>,
    pub pos: Pos,
    pub substmts: Vec<Stmt>,
}

impl Stmt {
    pub fn new(keyword: Keyword, arg: Option<String>, pos: Pos, substmts: Vec<Stmt>) -> Self {
        Stmt { keyword, arg, pos, substmts }
    }

    /// Returns the first substatement matching the given built-in keyword.
    pub fn get_substmt(&self, kw: BuiltInKeyword) -> Option<&Stmt> {
        self.substmts.iter().find(|s| s.keyword.is_builtin(kw))
    }

    /// Returns all substatements matching the given built-in keyword.
    pub fn get_substmts(&self, kw: BuiltInKeyword) -> impl Iterator<Item = &Stmt> {
        self.substmts.iter().filter(move |s| s.keyword.is_builtin(kw))
    }

    /// Convenience: arg as `&str` (panics if None — use only when arg is required by grammar).
    pub fn arg_str(&self) -> &str {
        self.arg.as_deref().unwrap_or("")
    }
}

// ── Module identity ───────────────────────────────────────────────────────────

/// A YANG revision date string, e.g. `"2024-01-15"`.
pub type Revision = String;

/// Identifies a YANG module by name and optional revision.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleKey {
    pub name: String,
    /// `None` means "latest available revision".
    pub revision: Option<Revision>,
}

impl ModuleKey {
    pub fn new(name: impl Into<String>, revision: Option<impl Into<String>>) -> Self {
        ModuleKey { name: name.into(), revision: revision.map(|r| r.into()) }
    }

    pub fn latest(name: impl Into<String>) -> Self {
        ModuleKey { name: name.into(), revision: None }
    }

    /// Order two YANG revision dates. An absent revision (`None`) sorts oldest;
    /// present revisions compare lexically, which — for the canonical
    /// `YYYY-MM-DD` form (RFC 7950 §7.1.9) — is chronological order. Used to
    /// resolve a revision-less `import`/`include` to the latest available
    /// revision (RFC 7950 §5.1.1).
    pub fn revision_cmp(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
        use std::cmp::Ordering::{Equal, Greater, Less};
        match (a, b) {
            (None, None) => Equal,
            (None, Some(_)) => Less,
            (Some(_), None) => Greater,
            (Some(x), Some(y)) => x.cmp(y),
        }
    }
}

impl fmt::Display for ModuleKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.revision {
            Some(rev) => write!(f, "{}@{}", self.name, rev),
            None => write!(f, "{}", self.name),
        }
    }
}

// ── Errors ────────────────────────────────────────────────────────────────────

/// Severity of a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Warning,
    Error,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Level::Warning => write!(f, "warning"),
            Level::Error => write!(f, "error"),
        }
    }
}

/// A diagnostic produced during any pipeline phase.
#[derive(Debug, Clone)]
pub struct YError {
    pub level: Level,
    pub pos: Pos,
    pub code: ErrorCode,
    /// Human-readable message, pre-formatted.
    pub message: String,
}

impl fmt::Display for YError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}: [{}] {}", self.pos, self.level, self.code, self.message)
    }
}

/// Symbolic error codes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    // Parse errors
    ParseUnexpectedChar,
    ParseUnterminatedString,
    ParseExpectedSemiOrBrace,
    ParseExpectedArgument,
    ParseUnknownKeyword,
    // Grammar errors
    GrammarUnexpectedStatement,
    GrammarMissingRequired,
    GrammarBadArgument,
    GrammarBadYangVersion,
    // Module resolution errors
    ModuleNotFound,
    ModuleCircularDependency,
    ModuleDuplicateRevision,
    // Semantic errors
    SemanticUnknownPrefix,
    SemanticUnknownType,
    SemanticUnknownGrouping,
    SemanticUnknownFeature,
    SemanticUnknownIdentity,
    SemanticInvalidLeafref,
    SemanticInvalidAugmentTarget,
    SemanticInvalidDeviationTarget,
    SemanticInvalidDefault,
    SemanticInvalidPattern,
    SemanticInvalidRange,
    SemanticXpathSyntax,
    SemanticXpathIllegalFunction,
    SemanticXpathUnknownPrefix,
    // Warnings
    WarnUnusedImport,
    WarnObsoleteStatus,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ErrorCode::ParseUnexpectedChar => "PARSE_UNEXPECTED_CHAR",
            ErrorCode::ParseUnterminatedString => "PARSE_UNTERMINATED_STRING",
            ErrorCode::ParseExpectedSemiOrBrace => "PARSE_EXPECTED_SEMI_OR_BRACE",
            ErrorCode::ParseExpectedArgument => "PARSE_EXPECTED_ARGUMENT",
            ErrorCode::ParseUnknownKeyword => "PARSE_UNKNOWN_KEYWORD",
            ErrorCode::GrammarUnexpectedStatement => "GRAMMAR_UNEXPECTED_STMT",
            ErrorCode::GrammarMissingRequired => "GRAMMAR_MISSING_REQUIRED",
            ErrorCode::GrammarBadArgument => "GRAMMAR_BAD_ARGUMENT",
            ErrorCode::GrammarBadYangVersion => "GRAMMAR_BAD_YANG_VERSION",
            ErrorCode::ModuleNotFound => "MODULE_NOT_FOUND",
            ErrorCode::ModuleCircularDependency => "MODULE_CIRCULAR_DEP",
            ErrorCode::ModuleDuplicateRevision => "MODULE_DUPLICATE_REVISION",
            ErrorCode::SemanticUnknownPrefix => "UNKNOWN_PREFIX",
            ErrorCode::SemanticUnknownType => "UNKNOWN_TYPE",
            ErrorCode::SemanticUnknownGrouping => "UNKNOWN_GROUPING",
            ErrorCode::SemanticUnknownFeature => "UNKNOWN_FEATURE",
            ErrorCode::SemanticUnknownIdentity => "UNKNOWN_IDENTITY",
            ErrorCode::SemanticInvalidLeafref => "INVALID_LEAFREF",
            ErrorCode::SemanticInvalidAugmentTarget => "INVALID_AUGMENT_TARGET",
            ErrorCode::SemanticInvalidDeviationTarget => "INVALID_DEVIATION_TARGET",
            ErrorCode::SemanticInvalidDefault => "INVALID_DEFAULT",
            ErrorCode::SemanticInvalidPattern => "INVALID_PATTERN",
            ErrorCode::SemanticInvalidRange => "INVALID_RANGE",
            ErrorCode::SemanticXpathSyntax => "XPATH_SYNTAX",
            ErrorCode::SemanticXpathIllegalFunction => "XPATH_ILLEGAL_FUNCTION",
            ErrorCode::SemanticXpathUnknownPrefix => "XPATH_UNKNOWN_PREFIX",
            ErrorCode::WarnUnusedImport => "WARN_UNUSED_IMPORT",
            ErrorCode::WarnObsoleteStatus => "WARN_OBSOLETE_STATUS",
        };
        write!(f, "{}", s)
    }
}
