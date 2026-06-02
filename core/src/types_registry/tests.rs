// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
use std::collections::HashSet;
use std::sync::Arc;

use super::*;
use crate::annindex::AnnotationIndex;
use crate::astannindex::AstAnnotationIndex;
use crate::ast::ModuleKey;
use crate::compiler::{compile_module, ExpansionCtx, ModuleRegistry};
use crate::cursor::{Cursor, QName};
use crate::devindex::DeviationIndex;
use crate::parser::parse_yang;

/// Compile every `(name, source)` into a registry (single wave; sources must be
/// given in dependency order).
fn registry_from(sources: &[(&str, &str)]) -> ModuleRegistry {
    let mut reg = ModuleRegistry::new();
    for (name, src) in sources {
        let (stmts, errs) = parse_yang(src, Arc::from("test.yang"));
        assert!(errs.is_empty(), "parse errors in {name}: {errs:?}");
        let stmt = stmts.into_iter().next().expect("module stmt");
        let compiled = compile_module(
            &ModuleKey::latest(*name),
            stmt,
            &reg,
            &DeviationIndex::default(),
            &AnnotationIndex::default(),
            &AstAnnotationIndex::default(),
        );
        reg.insert(Arc::new(compiled));
    }
    reg
}

/// Pull the `type` Stmt out of a named top-level leaf of a module.
fn leaf_type<'a>(reg: &'a ModuleRegistry, module: &str, leaf: &str) -> crate::ast::Stmt {
    let m = reg.resolve_import(module, None).unwrap();
    let node = m.children.iter().find(|n| n.name == leaf).unwrap();
    match &node.kind {
        crate::compiler::SchemaNodeKind::Leaf { type_stmt, .. }
        | crate::compiler::SchemaNodeKind::LeafList { type_stmt, .. } => type_stmt.clone(),
        _ => panic!("{leaf} is not a leaf"),
    }
}

#[test]
fn resolves_direct_builtin_with_restrictions() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  leaf a {
    type string { length "1..10"; pattern "[a-z]*"; }
  }
}
"#,
    )]);
    let tr = TypeRegistry::new(&reg);
    let t = tr.resolve(&leaf_type(&reg, "m", "a"), "m").unwrap();
    assert_eq!(t.base, BuiltInType::String);
    assert!(t.chain.is_empty());
    assert_eq!(t.restrictions.length.as_deref(), Some("1..10"));
    assert_eq!(t.restrictions.patterns, vec!["[a-z]*".to_string()]);
}

#[test]
fn flattens_local_typedef_chain_and_narrows() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  typedef base-str { type string { length "1..10"; pattern "[a-z]*"; } }
  typedef mid-str  { type base-str; }
  leaf a { type mid-str { length "1..5"; } }
}
"#,
    )]);
    let tr = TypeRegistry::new(&reg);
    let t = tr.resolve(&leaf_type(&reg, "m", "a"), "m").unwrap();
    assert_eq!(t.base, BuiltInType::String);
    // Chain is outermost-first, base built-in not listed.
    assert_eq!(
        t.chain,
        vec![
            ("m".to_string(), "mid-str".to_string()),
            ("m".to_string(), "base-str".to_string()),
        ]
    );
    // Most-derived length wins; pattern is inherited.
    assert_eq!(t.restrictions.length.as_deref(), Some("1..5"));
    assert_eq!(t.restrictions.patterns, vec!["[a-z]*".to_string()]);
}

#[test]
fn resolves_cross_module_typedef_via_prefix() {
    let reg = registry_from(&[
        (
            "dep",
            r#"
module dep {
  namespace "urn:dep";
  prefix d;
  typedef counter { type uint32 { range "0..100"; } }
}
"#,
        ),
        (
            "m",
            r#"
module m {
  namespace "urn:m";
  prefix m;
  import dep { prefix dp; }
  leaf a { type dp:counter; }
}
"#,
        ),
    ]);
    let tr = TypeRegistry::new(&reg);
    let t = tr.resolve(&leaf_type(&reg, "m", "a"), "m").unwrap();
    assert_eq!(t.base, BuiltInType::Uint32);
    assert_eq!(t.chain, vec![("dep".to_string(), "counter".to_string())]);
    assert_eq!(t.restrictions.range.as_deref(), Some("0..100"));
}

#[test]
fn resolves_union_members() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  typedef pct { type uint8 { range "0..100"; } }
  leaf a {
    type union {
      type pct;
      type enumeration { enum unbounded; }
    }
  }
}
"#,
    )]);
    let tr = TypeRegistry::new(&reg);
    let t = tr.resolve(&leaf_type(&reg, "m", "a"), "m").unwrap();
    assert_eq!(t.base, BuiltInType::Union);
    assert_eq!(t.union_members.len(), 2);
    assert_eq!(t.union_members[0].base, BuiltInType::Uint8);
    assert_eq!(t.union_members[0].chain, vec![("m".to_string(), "pct".to_string())]);
    assert_eq!(t.union_members[1].base, BuiltInType::Enumeration);
    assert_eq!(t.union_members[1].enums.len(), 1);
    assert_eq!(t.union_members[1].enums[0].name, "unbounded");
}

#[test]
fn keeps_leafref_path_raw() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  leaf a { type leafref { path "../b"; } }
  leaf b { type string; }
}
"#,
    )]);
    let tr = TypeRegistry::new(&reg);
    let t = tr.resolve(&leaf_type(&reg, "m", "a"), "m").unwrap();
    assert_eq!(t.base, BuiltInType::Leafref);
    assert_eq!(t.leafref_path.as_deref(), Some("../b"));
    assert!(t.require_instance);
}

#[test]
fn detects_typedef_cycle() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  typedef a { type b; }
  typedef b { type a; }
  leaf x { type a; }
}
"#,
    )]);
    let tr = TypeRegistry::new(&reg);
    let err = tr.resolve(&leaf_type(&reg, "m", "x"), "m").unwrap_err();
    assert!(matches!(err, TypeError::Cycle { .. }), "got {err:?}");
}

#[test]
fn unknown_prefix_is_an_error() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  leaf a { type nope:thing; }
}
"#,
    )]);
    let tr = TypeRegistry::new(&reg);
    let err = tr.resolve(&leaf_type(&reg, "m", "a"), "m").unwrap_err();
    assert!(matches!(err, TypeError::UnknownPrefix { .. }), "got {err:?}");
}

#[test]
fn resolves_enumeration_and_decimal64() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  leaf e { type enumeration { enum red { value 1; } enum green { value 5; } } }
  leaf d { type decimal64 { fraction-digits 3; range "0..9.999"; } }
}
"#,
    )]);
    let tr = TypeRegistry::new(&reg);
    let e = tr.resolve(&leaf_type(&reg, "m", "e"), "m").unwrap();
    assert_eq!(e.base, BuiltInType::Enumeration);
    assert_eq!(e.enums.len(), 2);
    assert_eq!(e.enums[0], EnumValue { name: "red".into(), value: Some(1) });
    assert_eq!(e.enums[1], EnumValue { name: "green".into(), value: Some(5) });

    let d = tr.resolve(&leaf_type(&reg, "m", "d"), "m").unwrap();
    assert_eq!(d.base, BuiltInType::Decimal64);
    assert_eq!(d.fraction_digits, Some(3));
    assert_eq!(d.restrictions.range.as_deref(), Some("0..9.999"));
}

#[test]
fn observer_fires_per_typedef_in_encounter_order() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  typedef base-str { type string; }
  typedef mid-str  { type base-str; }
  leaf a { type mid-str; }
}
"#,
    )]);
    let tr = TypeRegistry::new(&reg);

    struct Rec(Vec<String>);
    impl TypeResolutionObserver for Rec {
        fn on_typedef_resolved(&mut self, source_module: &str, typedef: &crate::compiler::Typedef) {
            self.0.push(format!("{source_module}:{}", typedef.name));
        }
    }
    let mut rec = Rec(Vec::new());
    tr.resolve_with_observer(&leaf_type(&reg, "m", "a"), "m", &mut rec)
        .unwrap();
    // Encounter order: outer typedef first, then its base.
    assert_eq!(rec.0, vec!["m:mid-str".to_string(), "m:base-str".to_string()]);
}

// ── #2 leafref target resolution ─────────────────────────────────────────────

fn ctx<'a>(reg: &'a ModuleRegistry, feats: &'a HashSet<(String, String)>) -> ExpansionCtx<'a> {
    ExpansionCtx::new(reg, feats).with_unlisted_modules_enabled()
}

/// Position a cursor at a leaf named by `path` (each element a logical child
/// name) and return `(cursor_at_leaf, that_leaf's_type_stmt)`.
fn cursor_at<'a>(
    cx: &'a ExpansionCtx<'a>,
    module: &'a crate::compiler::CompiledModule,
    path: &[&str],
) -> (Cursor<'a>, crate::ast::Stmt) {
    let mut cur = Cursor::root_of(module, cx);
    for name in path {
        cur.move_to_child(&QName::bare(*name)).unwrap();
    }
    let ts = match &cur.current().unwrap().kind {
        crate::compiler::SchemaNodeKind::Leaf { type_stmt, .. }
        | crate::compiler::SchemaNodeKind::LeafList { type_stmt, .. } => type_stmt.clone(),
        _ => panic!("not a leaf"),
    };
    (cur, ts)
}

#[test]
fn resolves_relative_leafref_to_string() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  container c {
    leaf ref { type leafref { path "../target"; } }
    leaf target { type string { length "1..8"; } }
  }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let (cur, ts) = cursor_at(&cx, &m, &["c", "ref"]);
    let tr = TypeRegistry::new(&reg);
    let lt = tr.follow_leafref(&ts, &cur).unwrap();
    assert_eq!(lt.target_node.name, "target");
    assert!(!lt.is_leaf_list);
    assert_eq!(lt.target_type.base, BuiltInType::String);
    assert_eq!(lt.target_type.restrictions.length.as_deref(), Some("1..8"));
}

#[test]
fn resolves_leafref_to_leaf_list_target() {
    // The doc's named failure: leaf-list targets must produce the target type,
    // not `{list, undefined}`.
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  leaf-list names { type string; }
  leaf ref { type leafref { path "/m:names"; } }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let (cur, ts) = cursor_at(&cx, &m, &["ref"]);
    let tr = TypeRegistry::new(&reg);
    let lt = tr.follow_leafref(&ts, &cur).unwrap();
    assert_eq!(lt.target_node.name, "names");
    assert!(lt.is_leaf_list, "target is a leaf-list");
    assert_eq!(lt.target_type.base, BuiltInType::String);
}

#[test]
fn leafref_sibling_through_transparent_case() {
    // `protocol` is a sibling of a choice; a leaf inside a case references it via
    // `../protocol`. With choice/case transparent, this resolves to `[.]`.
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  container c {
    leaf protocol { type string; }
    choice ch {
      case a { leaf ref { type leafref { path "../protocol"; } } }
    }
  }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let (cur, ts) = cursor_at(&cx, &m, &["c", "ref"]);
    let tr = TypeRegistry::new(&reg);
    let lt = tr.follow_leafref(&ts, &cur).unwrap();
    assert_eq!(lt.target_node.name, "protocol");
    assert_eq!(lt.target_type.base, BuiltInType::String);
}

#[test]
fn follows_leafref_chain_to_ultimate_base() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  leaf base { type uint32 { range "0..9"; } }
  leaf mid  { type leafref { path "/m:base"; } }
  leaf top  { type leafref { path "/m:mid"; } }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let (cur, ts) = cursor_at(&cx, &m, &["top"]);
    let tr = TypeRegistry::new(&reg);
    let lt = tr.follow_leafref(&ts, &cur).unwrap();
    // Immediate target is `mid`, but the resolved type is the ultimate base.
    assert_eq!(lt.target_node.name, "mid");
    assert_eq!(lt.target_type.base, BuiltInType::Uint32);
    assert_eq!(lt.target_type.restrictions.range.as_deref(), Some("0..9"));
}

#[test]
fn resolves_cross_module_leafref() {
    let reg = registry_from(&[
        (
            "dep",
            r#"
module dep {
  namespace "urn:dep";
  prefix d;
  container store { leaf id { type uint16; } }
}
"#,
        ),
        (
            "m",
            r#"
module m {
  namespace "urn:m";
  prefix m;
  import dep { prefix d; }
  leaf ref { type leafref { path "/d:store/d:id"; } }
}
"#,
        ),
    ]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let (cur, ts) = cursor_at(&cx, &m, &["ref"]);
    let tr = TypeRegistry::new(&reg);
    let lt = tr.follow_leafref(&ts, &cur).unwrap();
    assert_eq!(lt.target_node.name, "id");
    assert_eq!(lt.target_type.base, BuiltInType::Uint16);
}

#[test]
fn observer_fires_on_leafref_resolution_in_chain_order() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  leaf base { type string; }
  leaf mid  { type leafref { path "/m:base"; } }
  leaf top  { type leafref { path "/m:mid"; } }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let (cur, ts) = cursor_at(&cx, &m, &["top"]);
    let tr = TypeRegistry::new(&reg);

    #[derive(Default)]
    struct Rec {
        leafrefs: Vec<String>,
    }
    impl TypeResolutionObserver for Rec {
        fn on_leafref_resolved(&mut self, source_module: &str, target: &crate::compiler::SchemaNode) {
            self.leafrefs.push(format!("{source_module}:{}", target.name));
        }
    }
    let mut rec = Rec::default();
    tr.follow_leafref_with_observer(&ts, &cur, &mut rec).unwrap();
    // top → mid (resolved), then the chain follows mid → base (resolved).
    assert_eq!(rec.leafrefs, vec!["m:mid".to_string(), "m:base".to_string()]);
}

#[test]
fn observer_fires_for_outer_typedef_even_on_cache_hit() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  typedef t { type string; }
  leaf a { type t; }
  leaf b { type t; }
}
"#,
    )]);
    let tr = TypeRegistry::new(&reg);

    #[derive(Default)]
    struct Rec(usize);
    impl TypeResolutionObserver for Rec {
        fn on_typedef_resolved(&mut self, _m: &str, _t: &crate::compiler::Typedef) {
            self.0 += 1;
        }
    }
    let mut rec = Rec::default();
    // Resolve the same typedef twice; the value is memoized but each *encounter*
    // still fires the observer.
    tr.resolve_with_observer(&leaf_type(&reg, "m", "a"), "m", &mut rec).unwrap();
    tr.resolve_with_observer(&leaf_type(&reg, "m", "b"), "m", &mut rec).unwrap();
    assert_eq!(rec.0, 2);
}
