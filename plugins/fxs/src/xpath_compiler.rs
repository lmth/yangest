//! XPath compiler: converts parsed XPath AST to ConfD's internal XPath term format.
//!
//! ConfD stores compiled XPath in its internal AST format for efficient runtime
//! evaluation. This module produces those terms from yangest's parsed `XPathExpr`.
//!
//! The output format is the Erlang term encoding written to `.fxs` files, matching
//! what `yanger_fxs.erl` produces via ConfD's internal XPath compiler.

use eetf::{FixInteger, Float, ImproperList, Term};

use yangest_core::compiler::{CompiledModule, ExpansionCtx, MustExpr, SchemaNode, SchemaNodeKind, WhenExpr};
use yangest_core::xpath::{Axis, LocationPath, NodeTest, Step, XPathExpr, parse_xpath};

use crate::terms::{atom, binary_str, charlist, int, list, nil, tuple, undefined};

// ── Constants ─────────────────────────────────────────────────────────────────

/// `F_LOAD_FXS_MK_DEL_DEPS` — added to `{load_flags, N}` when a node has
/// dependency-based when/must expressions.
pub const F_LOAD_FXS_MK_DEL_DEPS: u32 = 1 << 13; // 8192

/// `F_V_DEPENDENCY` — vmfa flag: this vmfa has dependency paths.
const F_V_DEPENDENCY: u32 = 1 << 1; // 2

/// `F_V_NO_DEPENDENCY` — vmfa flag: this vmfa has no dependencies (implicit must).
const F_V_NO_DEPENDENCY: u32 = 1 << 8; // 256

/// `F_V_IGNORE_CREATE` — vmfa flag: ignore create events (common for must expressions
/// on nodes that can't be created independently).
const F_V_IGNORE_CREATE: u32 = 1 << 3; // 8

// ── Public API ────────────────────────────────────────────────────────────────

/// Context for resolving XPath prefixes to namespace URIs.
pub struct XPathCtx<'a> {
    /// Namespace URI of the module that owns the node/expression.
    pub module_ns: &'a str,
    /// Maps prefix → module name for the module that defines the expression.
    /// (Not prefix → NS directly; we need to resolve via registry.)
    prefix_map: &'a yangest_core::compiler::PrefixMap,
    /// Registry for resolving module name → `CompiledModule` (to get namespace).
    registry: &'a yangest_core::compiler::ModuleRegistry,
}

impl<'a> XPathCtx<'a> {
    pub fn new(
        module_ns: &'a str,
        prefix_map: &'a yangest_core::compiler::PrefixMap,
        registry: &'a yangest_core::compiler::ModuleRegistry,
    ) -> Self {
        XPathCtx {
            module_ns,
            prefix_map,
            registry,
        }
    }

    /// Resolves a prefix to its namespace URI.
    /// Falls back to `module_ns` for unprefixed names or unknown prefixes.
    fn resolve_prefix(&self, prefix: &str) -> String {
        if let Some(mod_name) = self.prefix_map.get(prefix) {
            if let Some(m) = self.registry.resolve_import(mod_name, None) {
                return m.namespace.clone();
            }
            return mod_name.clone();
        }
        self.module_ns.to_string()
    }

    /// Resolves a QName to `(namespace_uri, local_name)`.
    fn resolve_qname(&self, prefix: Option<&str>, local: &str) -> (String, String) {
        let ns = match prefix {
            Some(p) => self.resolve_prefix(p),
            None => self.module_ns.to_string(),
        };
        (ns, local.to_string())
    }
}

/// Builds the `extra` items for a schema node's `when` expressions.
///
/// Returns `(when_extra_items, load_flags_to_or_in)` where:
/// - `when_extra_items` is the `{'when', [...]}` term (if any)
/// - `load_flags_to_or_in` is `F_LOAD_FXS_MK_DEL_DEPS` if deps are present, else 0
pub fn build_when_extra(
    node: &SchemaNode,
    module: &CompiledModule,
    ctx: &ExpansionCtx<'_>,
) -> (Vec<Term>, u32) {
    if node.when.is_empty() {
        return (vec![], 0);
    }

    // Determine which module owns this when expression.
    let (when_ns, when_prefix_map, when_mod_name, when_rev) =
        resolve_node_module(node, module, ctx);
    let xpath_ctx = XPathCtx::new(&when_ns, &when_prefix_map, ctx.registry);

    let mut when_tuples: Vec<Term> = Vec::new();
    let mut has_deps = false;

    for when_expr in &node.when {
        if let Some((t, has_dep)) =
            compile_when_expr(when_expr, &xpath_ctx, &when_ns, &when_mod_name, &when_rev)
        {
            when_tuples.push(t);
            if has_dep {
                has_deps = true;
            }
        }
    }

    if when_tuples.is_empty() {
        return (vec![], 0);
    }

    let when_term = tuple(vec![atom("when"), list(when_tuples)]);
    let load_flags = if has_deps { F_LOAD_FXS_MK_DEL_DEPS } else { 0 };
    (vec![when_term], load_flags)
}

/// Builds the `validatemfas` list for a schema node's `must` expressions.
///
/// Returns `(vmfa_terms, load_flags_to_or_in, cs_flags_to_or_in)`.
pub fn build_must_vmfas(
    node: &SchemaNode,
    parent: Option<&SchemaNode>,
    module: &CompiledModule,
    ctx: &ExpansionCtx<'_>,
) -> (Vec<Term>, u32, u128) {
    let musts = node_musts(node);
    if musts.is_empty() {
        return (vec![], 0, 0);
    }

    let (must_ns, must_prefix_map, must_mod_name, must_rev) =
        resolve_node_module(node, module, ctx);
    let xpath_ctx = XPathCtx::new(&must_ns, &must_prefix_map, ctx.registry);

    let mut vmfas: Vec<Term> = Vec::new();
    let mut combined_load_flags: u32 = 0;

    for must_expr in musts {
        let (vmfa, load_flags) =
            compile_must_expr(must_expr, node, parent, &xpath_ctx, &must_ns, &must_mod_name, &must_rev, ctx);
        vmfas.push(vmfa);
        combined_load_flags |= load_flags;
    }

    (vmfas, combined_load_flags, 0)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Gets the musts list from a node's kind, if any.
fn node_musts(node: &SchemaNode) -> &[MustExpr] {
    use yangest_core::compiler::SchemaNodeKind::*;
    match &node.kind {
        Container { musts, .. } => musts,
        Leaf { musts, .. } => musts,
        LeafList { musts, .. } => musts,
        List { musts, .. } => musts,
        Rpc { musts, .. } => musts,
        Notification { musts, .. } => musts,
        AnyXml { musts, .. } => musts,
        AnyData { musts, .. } => musts,
        _ => &[],
    }
}

/// Resolves the defining module of a node, returning `(ns, prefix_map, mod_name, revision)`.
/// Falls back to `module` (the file module) if the node's module can't be resolved.
fn resolve_node_module<'a>(
    node: &SchemaNode,
    module: &'a CompiledModule,
    ctx: &'a ExpansionCtx<'_>,
) -> (
    String,
    yangest_core::compiler::PrefixMap,
    String,
    Option<String>,
) {
    // Try to look up the node's defining module in the registry.
    if node.module_name != module.key.name {
        if let Some(m) = ctx.registry.resolve_import(&node.module_name, None) {
            return (
                m.namespace.clone(),
                m.prefix_map.clone(),
                m.key.name.clone(),
                m.key.revision.clone(),
            );
        }
    }
    (
        module.namespace.clone(),
        module.prefix_map.clone(),
        module.key.name.clone(),
        module.key.revision.clone(),
    )
}

/// Compiles a single `WhenExpr` to a ConfD when-tuple.
/// Returns `Some((term, has_deps))` or `None` if the expression should be dropped
/// (e.g., always-true after partial evaluation — but we don't implement that).
fn compile_when_expr(
    when_expr: &WhenExpr,
    ctx: &XPathCtx<'_>,
    ns: &str,
    mod_name: &str,
    revision: &Option<String>,
) -> Option<(Term, bool)> {
    let parsed = match parse_xpath(&when_expr.xpath) {
        Ok(e) => e,
        Err(_) => return None,
    };

    let compiled = compile_expr(&parsed, ctx);
    // Outer xp_boolean wrapping (always added by yanger_fxs).
    let wrapped = tuple(vec![
        atom("function_call"),
        atom("xp_boolean"),
        list(vec![compiled]),
    ]);

    let orig_bin = binary_str(&when_expr.xpath);

    let deps = extract_deps(&parsed, ctx);
    let has_deps = !deps.is_empty();
    let deps_term = tuple(vec![list(deps), nil()]);

    let ns_to_prefix_map_id = ns_to_prefix_map_id_term(ns, mod_name, revision);

    let when_tuple = tuple(vec![
        wrapped,
        orig_bin,
        int(0), // Flags
        deps_term,
        int(0), // XPathRoot
        ns_to_prefix_map_id,
    ]);

    Some((when_tuple, has_deps))
}

/// Compiles a single `MustExpr` to a ConfD vmfa term.
fn compile_must_expr(
    must_expr: &MustExpr,
    node: &SchemaNode,
    parent: Option<&SchemaNode>,
    ctx: &XPathCtx<'_>,
    ns: &str,
    mod_name: &str,
    revision: &Option<String>,
    expansion_ctx: &ExpansionCtx<'_>,
) -> (Term, u32) {
    let parsed = match parse_xpath(&must_expr.xpath) {
        Ok(e) => e,
        Err(_) => {
            // Fallback: empty vmfa
            return (make_empty_vmfa(must_expr, ns, mod_name, revision), 0);
        }
    };

    let compiled = compile_expr(&parsed, ctx);
    let wrapped = tuple(vec![
        atom("function_call"),
        atom("xp_boolean"),
        list(vec![compiled]),
    ]);

    let orig_bin = binary_str(&must_expr.xpath);

    let err_msg = match &must_expr.error_message {
        Some(msg) => binary_str(msg),
        None => undefined(),
    };
    let err_tag = match &must_expr.error_app_tag {
        Some(tag) => binary_str(tag),
        None => binary_str("must-violation"),
    };

    let deps = extract_deps(&parsed, ctx);
    let has_deps = !deps.is_empty();

    // F_V_IGNORE_CREATE: set when has_deps AND the expression does NOT trigger create events.
    // Mirrors yanger_fxs get_v_ignore_create/4:
    // - xp_count / xp_position anywhere → false
    // - xp_not with non-local refs (parent steps to empty-leaf/list/leaf-list/p-container) → false
    let ignore_create = has_deps
        && !expr_has_count_or_position(&parsed)
        && not_args_are_local(&parsed, node, parent, expansion_ctx);

    // VFlags: F_V_DEPENDENCY if deps present, F_V_NO_DEPENDENCY if no deps.
    // Also add F_V_IGNORE_CREATE when applicable.
    let v_flags: u32 = if has_deps {
        F_V_DEPENDENCY | if ignore_create { F_V_IGNORE_CREATE } else { 0 }
    } else {
        F_V_NO_DEPENDENCY
    };

    let ns_to_prefix_map_id = ns_to_prefix_map_id_term(ns, mod_name, revision);

    // A = [compiled, orig, err_msg, err_tag, undefined, xpath_root, vflags, ns_to_prefix_map_id]
    let a = list(vec![
        wrapped,
        orig_bin,
        err_msg,
        err_tag,
        undefined(), // error_tag (always undefined in yanger)
        int(0),      // XPathRoot
        uint_from_u32(v_flags),
        ns_to_prefix_map_id,
    ]);

    let dep_terms: Vec<Term> = deps;
    let dep_list = list(dep_terms.clone());

    // vmfa tuple: {vmfa, m, f, a, flags, deps, del_deps, priority}
    let vmfa = tuple(vec![
        atom("vmfa"),
        atom("cs_validate"),
        atom("v_must"),
        a,
        uint_from_u32(v_flags), // vmfa flags
        dep_list,
        nil(),  // del_deps
        int(0), // priority = DEFAULT_VALIDATION_PRIORITY
    ]);

    let load_flags = if has_deps { F_LOAD_FXS_MK_DEL_DEPS } else { 0 };
    (vmfa, load_flags)
}

fn make_empty_vmfa(
    must_expr: &MustExpr,
    ns: &str,
    mod_name: &str,
    revision: &Option<String>,
) -> Term {
    let orig_bin = binary_str(&must_expr.xpath);
    let ns_to_prefix_map_id = ns_to_prefix_map_id_term(ns, mod_name, revision);
    let a = list(vec![
        nil(),
        orig_bin,
        undefined(),
        binary_str("must-violation"),
        undefined(),
        int(0),
        int(0),
        ns_to_prefix_map_id,
    ]);
    tuple(vec![
        atom("vmfa"),
        atom("cs_validate"),
        atom("v_must"),
        a,
        int(0),
        nil(),
        nil(),
        int(0),
    ])
}

/// Builds the `{NS_atom, ModName_atom, RevBin}` ns_to_prefix_map_id term.
fn ns_to_prefix_map_id_term(ns: &str, mod_name: &str, revision: &Option<String>) -> Term {
    let rev = match revision.as_deref() {
        Some(r) => binary_str(r),
        None => undefined(),
    };
    tuple(vec![atom(ns), atom(mod_name), rev])
}

// ── XPath expression compiler ─────────────────────────────────────────────────

/// Returns true if `expr` is `current()` with no arguments.
fn is_current_call(expr: &XPathExpr) -> bool {
    matches!(expr, XPathExpr::FunctionCall { name, args } if name.local == "current" && args.is_empty())
}

/// Compiles an operand of a comparison expression.
///
/// In ConfD's compiled XPath format, `current()` used as a path/node-set in a
/// comparison is encoded as a single-element list `[{function_call,xp_current,[]}]`
/// rather than the bare `{function_call,xp_current,[]}` form.  This matches
/// yanger_fxs's encoding of e.g. `current() <= ../max-free`.
fn compile_comp_operand(expr: &XPathExpr, ctx: &XPathCtx<'_>) -> Term {
    if is_current_call(expr) {
        list(vec![tuple(vec![
            atom("function_call"),
            atom("xp_current"),
            nil(),
        ])])
    } else {
        compile_expr(expr, ctx)
    }
}

fn compile_expr(expr: &XPathExpr, ctx: &XPathCtx<'_>) -> Term {
    match expr {
        XPathExpr::String(s) => tuple(vec![atom("literal"), charlist(s)]),
        XPathExpr::Number(n) => {
            let v = *n;
            if v.fract() == 0.0 && v >= i32::MIN as f64 && v <= i32::MAX as f64 {
                tuple(vec![atom("number"), int(v as i32)])
            } else {
                tuple(vec![atom("number"), Term::from(Float { value: v })])
            }
        }
        XPathExpr::Path(path) => compile_path(path, ctx),
        XPathExpr::FunctionCall { name, args } => {
            let fn_atom = format!("xp_{}", name.local);
            let compiled_args: Vec<Term> = args.iter().map(|a| compile_expr(a, ctx)).collect();
            tuple(vec![
                atom("function_call"),
                atom(&fn_atom),
                list(compiled_args),
            ])
        }
        XPathExpr::Union(l, r) => tuple(vec![
            atom("union"),
            compile_expr(l, ctx),
            compile_expr(r, ctx),
        ]),
        XPathExpr::Or(l, r) => tuple(vec![
            atom("bool"),
            atom("or"),
            compile_expr(l, ctx),
            compile_expr(r, ctx),
        ]),
        XPathExpr::And(l, r) => tuple(vec![
            atom("bool"),
            atom("and"),
            compile_expr(l, ctx),
            compile_expr(r, ctx),
        ]),
        XPathExpr::Eq(l, r) => tuple(vec![
            atom("comp"),
            atom("="),
            compile_comp_operand(l, ctx),
            compile_comp_operand(r, ctx),
        ]),
        XPathExpr::Ne(l, r) => tuple(vec![
            atom("comp"),
            atom("!="),
            compile_comp_operand(l, ctx),
            compile_comp_operand(r, ctx),
        ]),
        XPathExpr::Lt(l, r) => tuple(vec![
            atom("comp"),
            atom("<"),
            compile_comp_operand(l, ctx),
            compile_comp_operand(r, ctx),
        ]),
        XPathExpr::Gt(l, r) => tuple(vec![
            atom("comp"),
            atom(">"),
            compile_comp_operand(l, ctx),
            compile_comp_operand(r, ctx),
        ]),
        XPathExpr::Le(l, r) => tuple(vec![
            atom("comp"),
            atom("<="),
            compile_comp_operand(l, ctx),
            compile_comp_operand(r, ctx),
        ]),
        XPathExpr::Ge(l, r) => tuple(vec![
            atom("comp"),
            atom(">="),
            compile_comp_operand(l, ctx),
            compile_comp_operand(r, ctx),
        ]),
        XPathExpr::Add(l, r) => tuple(vec![
            atom("arith"),
            atom("+"),
            compile_expr(l, ctx),
            compile_expr(r, ctx),
        ]),
        XPathExpr::Sub(l, r) => tuple(vec![
            atom("arith"),
            atom("-"),
            compile_expr(l, ctx),
            compile_expr(r, ctx),
        ]),
        XPathExpr::Mul(l, r) => tuple(vec![
            atom("arith"),
            atom("*"),
            compile_expr(l, ctx),
            compile_expr(r, ctx),
        ]),
        XPathExpr::Div(l, r) => tuple(vec![
            atom("arith"),
            atom("div"),
            compile_expr(l, ctx),
            compile_expr(r, ctx),
        ]),
        XPathExpr::Mod(l, r) => tuple(vec![
            atom("arith"),
            atom("mod"),
            compile_expr(l, ctx),
            compile_expr(r, ctx),
        ]),
        XPathExpr::Neg(e) => tuple(vec![
            atom("arith"),
            atom("neg"),
            compile_expr(e, ctx),
            int(0),
        ]),
        XPathExpr::Variable(q) => {
            let fn_atom = match &q.prefix {
                Some(p) => format!("xp_var_{}_{}", p, q.local),
                None => format!("xp_var_{}", q.local),
            };
            tuple(vec![atom("function_call"), atom(&fn_atom), nil()])
        }
        XPathExpr::Filter(base, preds) => {
            // Filter expression: base[pred1][pred2]...
            let compiled_preds: Vec<Term> = preds
                .iter()
                .map(|p| tuple(vec![atom("pred"), compile_expr(p, ctx)]))
                .collect();
            tuple(vec![
                atom("filter"),
                compile_expr(base, ctx),
                list(compiled_preds),
            ])
        }
    }
}

fn compile_path(path: &LocationPath, ctx: &XPathCtx<'_>) -> Term {
    let kind = if path.absolute {
        atom("absolute")
    } else {
        atom("relative")
    };
    let steps: Vec<Term> = path.steps.iter().map(|s| compile_step(s, ctx)).collect();
    tuple(vec![kind, list(steps)])
}

fn compile_step(step: &Step, ctx: &XPathCtx<'_>) -> Term {
    let axis = compile_axis(step.axis);
    let node_test = compile_node_test(&step.node_test, ctx);
    let preds: Vec<Term> = step
        .predicates
        .iter()
        .map(|p| tuple(vec![atom("pred"), compile_expr(p, ctx)]))
        .collect();
    tuple(vec![atom("step"), axis, node_test, list(preds)])
}

fn compile_axis(axis: Axis) -> Term {
    match axis {
        Axis::Child => atom("child"),
        Axis::Parent => atom("parent"),
        Axis::Self_ => atom("self"),
        Axis::Attribute => atom("attribute"),
        Axis::Descendant => atom("descendant"),
        Axis::DescendantOrSelf => atom("descendant"),
        Axis::Ancestor => atom("ancestor"),
        Axis::AncestorOrSelf => atom("ancestor-or-self"),
        Axis::Following => atom("following"),
        Axis::FollowingSibling => atom("following-sibling"),
        Axis::Preceding => atom("preceding"),
        Axis::PrecedingSibling => atom("preceding-sibling"),
        Axis::Namespace => atom("namespace"),
    }
}

fn compile_node_test(nt: &NodeTest, ctx: &XPathCtx<'_>) -> Term {
    match nt {
        NodeTest::Name(q) => {
            let (ns, local) = ctx.resolve_qname(q.prefix.as_deref(), &q.local);
            tuple(vec![atom("name"), atom(&ns), atom(&local)])
        }
        NodeTest::Wildcard => atom("wildcard"),
        NodeTest::PrefixWildcard(p) => {
            let ns = ctx.resolve_prefix(p);
            tuple(vec![atom("name"), atom(&ns), atom("*")])
        }
        NodeTest::Node => tuple(vec![atom("node_type"), atom("node")]),
        NodeTest::Text => tuple(vec![atom("node_type"), atom("text")]),
        NodeTest::Comment => tuple(vec![atom("node_type"), atom("comment")]),
        NodeTest::Pi(Some(target)) => tuple(vec![
            atom("node_type"),
            atom("processing-instruction"),
            charlist(target),
        ]),
        NodeTest::Pi(None) => tuple(vec![atom("node_type"), atom("processing-instruction")]),
    }
}

// ── Dependency extraction ─────────────────────────────────────────────────────

/// Extracts the dependency paths from an XPath expression.
/// Each dep path is a list like `['..',['NS'|'local']]` or `['.']`.
pub fn extract_deps(expr: &XPathExpr, ctx: &XPathCtx<'_>) -> Vec<Term> {
    let mut raw_deps: Vec<Vec<Term>> = Vec::new();
    collect_deps(expr, ctx, &mut raw_deps);
    // Deduplicate (simple structural equality check).
    let mut seen: Vec<Vec<Term>> = Vec::new();
    for dep in raw_deps {
        if !seen.contains(&dep) {
            seen.push(dep);
        }
    }
    // Sort using Erlang term comparison order, matching yang_xpath_deps.erl
    // which collects deps into a map trie and iterates with maps:keys (sorted order).
    seen.sort_by(|a, b| cmp_dep_path(a, b));
    seen.into_iter().map(list).collect()
}

/// Compare two dependency paths using Erlang's term ordering.
///
/// Erlang's term ordering for atoms: alphabetical (byte comparison of atom name).
/// For tuples: element-by-element comparison.
/// For lists: element-by-element, then by length.
///
/// This mirrors the ordering that `maps:keys` produces in yang_xpath_deps.erl
/// when building the dependency trie.
fn cmp_dep_path(a: &[Term], b: &[Term]) -> std::cmp::Ordering {
    for (ta, tb) in a.iter().zip(b.iter()) {
        let ord = cmp_term(ta, tb);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

fn cmp_term(a: &Term, b: &Term) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    // Erlang type ordering: number < atom < reference < fun < port < pid < tuple < map < list < binary
    fn type_rank(t: &Term) -> u8 {
        match t {
            Term::FixInteger(_) | Term::BigInteger(_) | Term::Float(_) => 0,
            Term::Atom(_) => 1,
            Term::Reference(_) => 2,
            Term::Port(_) => 4,
            Term::Pid(_) => 5,
            Term::Tuple(_) => 6,
            Term::List(_) | Term::ImproperList(_) => 8,
            Term::Binary(_) => 9,
            _ => 10,
        }
    }

    let ra = type_rank(a);
    let rb = type_rank(b);
    if ra != rb {
        return ra.cmp(&rb);
    }

    match (a, b) {
        (Term::Atom(aa), Term::Atom(ab)) => aa.name.as_bytes().cmp(ab.name.as_bytes()),
        (Term::FixInteger(ai), Term::FixInteger(bi)) => ai.value.cmp(&bi.value),
        (Term::Tuple(at), Term::Tuple(bt)) => {
            let alen = at.elements.len();
            let blen = bt.elements.len();
            if alen != blen {
                return alen.cmp(&blen);
            }
            for (ea, eb) in at.elements.iter().zip(bt.elements.iter()) {
                let ord = cmp_term(ea, eb);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        }
        (Term::ImproperList(al), Term::ImproperList(bl)) => {
            for (ea, eb) in al.elements.iter().zip(bl.elements.iter()) {
                let ord = cmp_term(ea, eb);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            let alen = al.elements.len();
            let blen = bl.elements.len();
            if alen != blen {
                return alen.cmp(&blen);
            }
            cmp_term(&al.last, &bl.last)
        }
        (Term::List(al), Term::List(bl)) => {
            for (ea, eb) in al.elements.iter().zip(bl.elements.iter()) {
                let ord = cmp_term(ea, eb);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            al.elements.len().cmp(&bl.elements.len())
        }
        (Term::Binary(ab), Term::Binary(bb)) => ab.bytes.cmp(&bb.bytes),
        _ => Ordering::Equal,
    }
}

fn collect_deps(expr: &XPathExpr, ctx: &XPathCtx<'_>, deps: &mut Vec<Vec<Term>>) {
    match expr {
        XPathExpr::Path(path) if !path.absolute => {
            if let Some(dep) = relative_path_to_dep(path, ctx) {
                deps.push(dep);
            }
        }
        XPathExpr::FunctionCall { args, .. } => {
            for a in args {
                collect_deps(a, ctx, deps);
            }
        }
        XPathExpr::Or(l, r)
        | XPathExpr::And(l, r)
        | XPathExpr::Add(l, r)
        | XPathExpr::Sub(l, r)
        | XPathExpr::Mul(l, r)
        | XPathExpr::Div(l, r)
        | XPathExpr::Mod(l, r)
        | XPathExpr::Union(l, r) => {
            collect_deps(l, ctx, deps);
            collect_deps(r, ctx, deps);
        }
        XPathExpr::Eq(l, r)
        | XPathExpr::Ne(l, r)
        | XPathExpr::Lt(l, r)
        | XPathExpr::Gt(l, r)
        | XPathExpr::Le(l, r)
        | XPathExpr::Ge(l, r) => {
            // In a comparison, current() as a direct operand contributes a ['.'] dep
            // (the current node being validated is a dependency).
            collect_deps_comp_operand(l, ctx, deps);
            collect_deps_comp_operand(r, ctx, deps);
        }
        XPathExpr::Neg(e) => collect_deps(e, ctx, deps),
        XPathExpr::Filter(base, preds) => {
            collect_deps(base, ctx, deps);
            for p in preds {
                collect_deps(p, ctx, deps);
            }
        }
        _ => {}
    }
}

/// Collects deps for a comparison operand.
///
/// `current()` as a direct comparison operand contributes `['.']` — the current
/// node being validated is itself a dependency.  All other expressions are
/// handled by the regular `collect_deps`.
fn collect_deps_comp_operand(expr: &XPathExpr, ctx: &XPathCtx<'_>, deps: &mut Vec<Vec<Term>>) {
    if is_current_call(expr) {
        deps.push(vec![atom(".")]);
    } else {
        collect_deps(expr, ctx, deps);
    }
}

/// Converts a relative location path to its dependency representation.
///
/// The dep path encoding:
/// - `['..','..',['NS'|'local']]` for `../../name` (N parent steps + final named child)
/// - `['..']` for a path that goes to parent with no further named step
/// - `['.']` for current-level or descendant paths
fn relative_path_to_dep(path: &LocationPath, ctx: &XPathCtx<'_>) -> Option<Vec<Term>> {
    if path.steps.is_empty() {
        return None;
    }

    let first = &path.steps[0];
    match first.axis {
        Axis::Parent => {
            // Count consecutive parent steps at the start of the path.
            let parent_count = path
                .steps
                .iter()
                .take_while(|s| s.axis == Axis::Parent)
                .count();
            let mut dep: Vec<Term> = (0..parent_count).map(|_| atom("..")).collect();
            // Look for the first named step after all parent steps.
            for step in &path.steps[parent_count..] {
                if let NodeTest::Name(q) = &step.node_test {
                    let (ns, local) = ctx.resolve_qname(q.prefix.as_deref(), &q.local);
                    let ns_local = Term::from(ImproperList {
                        elements: vec![atom(&ns)],
                        last: Box::new(atom(&local)),
                    });
                    dep.push(ns_local);
                    return Some(dep);
                }
            }
            Some(dep)
        }
        Axis::Self_ | Axis::Child | Axis::Descendant | Axis::DescendantOrSelf => {
            Some(vec![atom(".")])
        }
        _ => None,
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn uint_from_u32(n: u32) -> Term {
    Term::from(FixInteger { value: n as i32 })
}

/// Returns true if the expression contains a `count()` or `position()` function
/// call anywhere in its subtree.  Used to determine whether `F_V_IGNORE_CREATE`
/// should be set: yanger sets it only when neither function appears.
fn expr_has_count_or_position(expr: &XPathExpr) -> bool {
    match expr {
        XPathExpr::FunctionCall { name, args } => {
            name.local == "count"
                || name.local == "position"
                || args.iter().any(expr_has_count_or_position)
        }
        XPathExpr::Path(lp) => lp
            .steps
            .iter()
            .any(|s| s.predicates.iter().any(expr_has_count_or_position)),
        XPathExpr::Filter(e, preds) => {
            expr_has_count_or_position(e) || preds.iter().any(expr_has_count_or_position)
        }
        XPathExpr::Union(a, b)
        | XPathExpr::Or(a, b)
        | XPathExpr::And(a, b)
        | XPathExpr::Eq(a, b)
        | XPathExpr::Ne(a, b)
        | XPathExpr::Lt(a, b)
        | XPathExpr::Gt(a, b)
        | XPathExpr::Le(a, b)
        | XPathExpr::Ge(a, b)
        | XPathExpr::Add(a, b)
        | XPathExpr::Sub(a, b)
        | XPathExpr::Mul(a, b)
        | XPathExpr::Div(a, b)
        | XPathExpr::Mod(a, b) => {
            expr_has_count_or_position(a) || expr_has_count_or_position(b)
        }
        XPathExpr::Neg(e) => expr_has_count_or_position(e),
        XPathExpr::String(_) | XPathExpr::Number(_) | XPathExpr::Variable(_) => false,
    }
}

/// Returns true if all `not()` calls in the expression have "local" references —
/// i.e., all paths inside `not()` stay within the current instance.
///
/// Mirrors `yanger_fxs:get_v_ignore_create/4` which uses a cursor to follow
/// paths and detect non-local references. Our simplified check:
/// - Absolute paths inside `not()` → non-local
/// - Paths going directly to an empty leaf / presence container / list → non-local
fn not_args_are_local(
    expr: &XPathExpr,
    node: &SchemaNode,
    parent: Option<&SchemaNode>,
    ctx: &ExpansionCtx<'_>,
) -> bool {
    match expr {
        XPathExpr::FunctionCall { name, args } if name.local == "not" => {
            args.iter().all(|arg| not_path_arg_is_local(arg, node, parent, ctx))
        }
        XPathExpr::FunctionCall { args, .. } => {
            args.iter().all(|a| not_args_are_local(a, node, parent, ctx))
        }
        XPathExpr::Path(lp) => lp
            .steps
            .iter()
            .all(|s| s.predicates.iter().all(|p| not_args_are_local(p, node, parent, ctx))),
        XPathExpr::Filter(e, preds) => {
            not_args_are_local(e, node, parent, ctx)
                && preds.iter().all(|p| not_args_are_local(p, node, parent, ctx))
        }
        XPathExpr::Union(a, b)
        | XPathExpr::Or(a, b)
        | XPathExpr::And(a, b)
        | XPathExpr::Eq(a, b)
        | XPathExpr::Ne(a, b)
        | XPathExpr::Lt(a, b)
        | XPathExpr::Gt(a, b)
        | XPathExpr::Le(a, b)
        | XPathExpr::Ge(a, b)
        | XPathExpr::Add(a, b)
        | XPathExpr::Sub(a, b)
        | XPathExpr::Mul(a, b)
        | XPathExpr::Div(a, b)
        | XPathExpr::Mod(a, b) => {
            not_args_are_local(a, node, parent, ctx) && not_args_are_local(b, node, parent, ctx)
        }
        XPathExpr::Neg(e) => not_args_are_local(e, node, parent, ctx),
        XPathExpr::String(_) | XPathExpr::Number(_) | XPathExpr::Variable(_) => true,
    }
}

/// Check whether a single path argument inside `not()` is "local".
fn not_path_arg_is_local(
    arg: &XPathExpr,
    node: &SchemaNode,
    parent: Option<&SchemaNode>,
    ctx: &ExpansionCtx<'_>,
) -> bool {
    match arg {
        XPathExpr::Path(lp) => {
            if lp.absolute {
                return false;
            }
            follow_path_is_local(&lp.steps, node, parent, ctx)
        }
        _ => not_args_are_local(arg, node, parent, ctx),
    }
}

/// Follow a relative path from `node` and return true only if the path is
/// "local".  Mirrors `follow_path0` / `follow_path1` in yanger_fxs:
/// - One parent step is allowed if the current node is NOT a list
///   (yanger passes `NumAllowList=1` for non-list nodes, `0` for list nodes).
/// - After a parent step, checks sibling children against the provided parent node.
/// - Absolute paths are always non-local.
fn follow_path_is_local(
    steps: &[Step],
    node: &SchemaNode,
    parent: Option<&SchemaNode>,
    ctx: &ExpansionCtx<'_>,
) -> bool {
    // Determine initial parent-step budget (mirrors yanger's follow_path0).
    let is_list = matches!(node.kind, SchemaNodeKind::List { .. });
    let initial_budget: i32 = if is_list { 0 } else { 1 };
    follow_path_is_local_inner(steps, node, parent, ctx, initial_budget)
}

fn follow_path_is_local_inner(
    steps: &[Step],
    node: &SchemaNode,
    parent: Option<&SchemaNode>,
    ctx: &ExpansionCtx<'_>,
    mut parent_budget: i32,
) -> bool {
    let mut current = node.clone();
    // Track the parent of `current` so we can navigate upward.
    let mut current_parent: Option<SchemaNode> = parent.cloned();

    for (i, step) in steps.iter().enumerate() {
        match step.axis {
            Axis::Self_ => {}
            Axis::Parent | Axis::Ancestor | Axis::AncestorOrSelf => {
                if parent_budget <= 0 {
                    return false;
                }
                parent_budget -= 1;
                if let Some(p) = current_parent.take() {
                    // Move current up to the parent; we no longer know the grandparent.
                    current = p;
                    current_parent = None;
                } else {
                    // No parent info — can't navigate upward.
                    return false;
                }
            }
            Axis::Child => {
                let child_name = match &step.node_test {
                    NodeTest::Name(q) => q.local.as_str(),
                    _ => return false,
                };
                match current.find_child(child_name, ctx) {
                    Some(child) => {
                        if i == steps.len() - 1 && is_empty_leaf_or_list(&child) {
                            return false;
                        }
                        current_parent = Some(current.clone());
                        current = child;
                    }
                    None => return false,
                }
            }
            _ => return false,
        }
    }

    true
}

/// Returns true if the node is an empty leaf, list, leaf-list, or presence
/// container — all of which cause `F_V_IGNORE_CREATE = false` in yanger.
fn is_empty_leaf_or_list(node: &SchemaNode) -> bool {
    match &node.kind {
        SchemaNodeKind::Leaf { type_stmt, .. } => {
            type_stmt.arg.as_deref() == Some("empty")
        }
        SchemaNodeKind::Container { presence, .. } => presence.is_some(),
        SchemaNodeKind::List { .. } | SchemaNodeKind::LeafList { .. } => true,
        _ => false,
    }
}