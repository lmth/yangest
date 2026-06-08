// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
use std::collections::HashSet;
use std::sync::Arc;

use super::*;
use crate::annindex::AnnotationIndex;
use crate::astannindex::AstAnnotationIndex;
use crate::ast::ModuleKey;
use crate::compiler::{compile_module, ModuleRegistry};
use crate::devindex::DeviationIndex;
use crate::parser::parse_yang;

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

/// Build a no-overlay, all-features-enabled ctx (bundle-like).
fn ctx<'a>(reg: &'a ModuleRegistry, features: &'a HashSet<(String, String)>) -> ExpansionCtx<'a> {
    ExpansionCtx::new(reg, features).with_unlisted_modules_enabled()
}

#[test]
fn navigates_nested_containers() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  container top {
    container mid {
      leaf x { type string; }
    }
  }
}
"#,
    )]);
    let feats = HashSet::new();
    let c = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let mut cur = Cursor::root_of(&m, &c);
    assert!(cur.at_root());
    assert!(cur.current().is_none());

    cur.move_to_child(&QName::bare("top")).unwrap();
    assert_eq!(cur.current().unwrap().name, "top");
    cur.follow_path(&[
        Axis::Child(QName::bare("mid")),
        Axis::Child(QName::bare("x")),
    ])
    .unwrap();
    assert_eq!(cur.current().unwrap().name, "x");
    assert_eq!(cur.depth(), 3);

    // ancestors are root-first, excluding current.
    let anc: Vec<&str> = cur.ancestors().map(|n| n.name.as_str()).collect();
    assert_eq!(anc, vec!["top", "mid"]);
    assert_eq!(cur.parent().unwrap().name, "mid");

    // `..` to mid, then back down.
    cur.move_step(&Axis::Parent).unwrap();
    assert_eq!(cur.current().unwrap().name, "mid");
}

#[test]
fn choice_and_case_are_transparent() {
    // `x` and `y` live inside cases of a choice; a sibling `protocol` is a direct
    // child of the container. All three must be logical siblings.
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  container c {
    leaf protocol { type string; }
    choice ch {
      case a { leaf x { type string; } }
      case b { leaf y { type string; } }
    }
  }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let mut cur = Cursor::root_of(&m, &cx);
    cur.move_to_child(&QName::bare("c")).unwrap();

    // `x`, `y` and `protocol` are all reachable as direct logical children of `c`.
    for name in ["protocol", "x", "y"] {
        assert!(
            cur.find_child(&QName::bare(name)).is_some(),
            "logical child '{name}' should be reachable through transparent choice/case"
        );
    }
    // The choice and case names themselves are NOT addressable.
    assert!(cur.find_child(&QName::bare("ch")).is_none());
    assert!(cur.find_child(&QName::bare("a")).is_none());

    // From `x` (inside case a), the logical parent is `c`, so `..` then `protocol`
    // resolves — i.e. a sibling reference across the transparent choice is `[.]`,
    // not `[.., <unknown>]`.
    let mut at_x = cur.clone();
    at_x.move_to_child(&QName::bare("x")).unwrap();
    assert_eq!(at_x.parent().unwrap().name, "c");
    at_x.move_step(&Axis::Parent).unwrap();
    assert_eq!(at_x.current().unwrap().name, "c");
    at_x.move_to_child(&QName::bare("protocol")).unwrap();
    assert_eq!(at_x.current().unwrap().name, "protocol");
}

#[test]
fn navigates_through_uses_expansion() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  grouping g {
    leaf a { type string; }
    container inner { leaf b { type string; } }
  }
  container host {
    uses g;
  }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let mut cur = Cursor::root_of(&m, &cx);
    cur.move_to_child(&QName::bare("host")).unwrap();
    // `uses g` is transparent: a and inner are direct logical children of host.
    cur.find_child(&QName::bare("a")).expect("leaf from grouping");
    cur.follow_path(&[
        Axis::Child(QName::bare("inner")),
        Axis::Child(QName::bare("b")),
    ])
    .unwrap();
    assert_eq!(cur.current().unwrap().name, "b");
}

#[test]
fn parent_at_root_errors_and_set_xpath_root_pins() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  container a { container b { leaf c { type string; } } }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let mut cur = Cursor::root_of(&m, &cx);
    assert_eq!(cur.move_step(&Axis::Parent), Err(CursorError::AtRoot));

    cur.move_to_child(&QName::bare("a")).unwrap();
    cur.move_to_child(&QName::bare("b")).unwrap();
    cur.set_xpath_root(); // pin at `b`
    cur.move_to_child(&QName::bare("c")).unwrap();
    cur.move_step(&Axis::Parent).unwrap(); // back to b (allowed)
    assert_eq!(cur.current().unwrap().name, "b");
    // Cannot go above the pinned root.
    assert_eq!(cur.move_step(&Axis::Parent), Err(CursorError::AtRoot));

    cur.reset_to_root();
    assert!(cur.at_root());
}

#[test]
fn floating_cursor_tracks_multiple_positions() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  container c1 { leaf shared { type string; } }
  container c2 { leaf shared { type string; } leaf only2 { type string; } }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let base = Cursor::root_of(&m, &cx);
    let c1 = base.find_child(&QName::bare("c1")).unwrap();
    let c2 = base.find_child(&QName::bare("c2")).unwrap();

    let mut float = FloatingCursor::from_positions(vec![c1, c2]);
    // `shared` exists under both → both survive.
    float.move_step(&Axis::Child(QName::bare("shared")));
    assert_eq!(float.positions().len(), 2);

    // `only2` exists under c2 only → from the two `shared` leaves it fails on both.
    let mut float2 = FloatingCursor::from_positions(
        base
            .find_child(&QName::bare("c1"))
            .into_iter()
            .chain(base.find_child(&QName::bare("c2")))
            .collect(),
    );
    float2.move_step(&Axis::Child(QName::bare("only2")));
    assert_eq!(float2.positions().len(), 1);
    assert_eq!(float2.positions()[0].current().unwrap().name, "only2");
}

#[test]
fn qualified_name_matches_namespace_module() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  leaf top { type string; }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let cur = Cursor::root_of(&m, &cx);
    assert!(cur.find_child(&QName::qualified("m", "top")).is_some());
    assert!(cur.find_child(&QName::qualified("other", "top")).is_none());
}

#[test]
fn child_nodes_structural_preserves_choice_and_case() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  container c {
    leaf protocol { type string; }
    choice ch {
      case a { leaf x { type string; } }
      case b { leaf y { type string; } }
    }
  }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let mut cur = Cursor::root_of(&m, &cx);
    cur.move_to_child(&QName::bare("c")).unwrap();

    // Flattened view (child_nodes): choice/case spliced away — protocol/x/y are
    // siblings and `ch` is gone.
    let flat: Vec<String> = cur.child_nodes().iter().map(|n| n.name.clone()).collect();
    assert!(flat.iter().any(|n| n == "protocol"));
    assert!(flat.iter().any(|n| n == "x"));
    assert!(flat.iter().any(|n| n == "y"));
    assert!(!flat.iter().any(|n| n == "ch"));

    // Structural view: the `choice` survives as a Choice node; the case leaves
    // are NOT direct children (they live inside its cases).
    let structural = cur.child_nodes_structural();
    let names: Vec<String> = structural.iter().map(|n| n.name.clone()).collect();
    assert!(names.iter().any(|n| n == "protocol"));
    let ch = structural
        .iter()
        .find(|n| n.name == "ch")
        .expect("structural view must keep the choice node");
    assert!(
        matches!(ch.kind, crate::compiler::SchemaNodeKind::Choice { .. }),
        "ch must be preserved as a Choice variant"
    );
    assert!(
        !names.iter().any(|n| n == "x") && !names.iter().any(|n| n == "y"),
        "case leaves must not be direct structural children"
    );
}

#[test]
fn child_nodes_structural_composes_external_augment_and_uses_body() {
    // The structural view must still include cross-module augments — including
    // the §11 case where the augment body is `uses grouping;` (the property a
    // plugin cannot reproduce without core's expand_augment_body).
    let reg = registry_from(&[
        (
            "base",
            r#"
module base {
  namespace "urn:base";
  prefix b;
  container c { leaf own { type string; } }
}
"#,
        ),
        (
            "aug",
            r#"
module aug {
  namespace "urn:aug";
  prefix a;
  import base { prefix b; }
  grouping g {
    leaf added { type uint32; }
    container extra { leaf deep { type string; } }
  }
  augment "/b:c" { uses g; }
}
"#,
        ),
    ]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("base", None).unwrap();
    let mut cur = Cursor::root_of(&m, &cx);
    cur.move_to_child(&QName::bare("c")).unwrap();

    let names: Vec<String> = cur
        .child_nodes_structural()
        .iter()
        .map(|n| n.name.clone())
        .collect();
    assert!(names.iter().any(|n| n == "own"), "own child present: {names:?}");
    assert!(
        names.iter().any(|n| n == "added"),
        "augment body `uses g` leaf must be composed structurally: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "extra"),
        "augment body `uses g` container must be composed structurally: {names:?}"
    );
}
