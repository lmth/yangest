// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
use std::collections::HashSet;
use std::sync::Arc;

use indexmap::IndexMap;

use super::*;
use crate::annindex::AnnotationIndex;
use crate::astannindex::AstAnnotationIndex;
use crate::ast::ModuleKey;
use crate::compiler::{compile_module, CompiledModule, ExpansionCtx, ModuleRegistry};
use crate::cursor::{Cursor, QName as CQName};
use crate::devindex::DeviationIndex;
use crate::parser::{parse_yang};
use crate::xpath::parse_xpath;

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

fn ctx<'a>(reg: &'a ModuleRegistry, feats: &'a HashSet<(String, String)>) -> ExpansionCtx<'a> {
    ExpansionCtx::new(reg, feats).with_unlisted_modules_enabled()
}

/// A simple namespace context where `prefix == module name`.
fn ns(default_module: &str, prefixes: &[(&str, &str)]) -> NamespaceCtx {
    let mut m = IndexMap::new();
    for (p, mo) in prefixes {
        m.insert((*p).to_string(), (*mo).to_string());
    }
    NamespaceCtx { prefixes: m, default_module: default_module.to_string() }
}

fn cursor_to<'a>(
    cx: &'a ExpansionCtx<'a>,
    module: &'a CompiledModule,
    path: &[&str],
) -> Cursor<'a> {
    let mut cur = Cursor::root_of(module, cx);
    for n in path {
        cur.move_to_child(&CQName::bare(*n)).unwrap();
    }
    cur
}

#[test]
fn set_default_namespace_uses_defining_module_for_unprefixed() {
    let unpref = crate::xpath::QName::bare("x");
    assert_eq!(
        set_default_namespace(&unpref, "defmod", "curmod").as_deref(),
        Some("defmod")
    );
    let pref = crate::xpath::QName::qualified("p", "x");
    assert_eq!(set_default_namespace(&pref, "defmod", "curmod"), None);
}

#[test]
fn normalises_relative_sibling_path() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  container c {
    leaf a { type string; }
    leaf b { type string; }
  }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let cur = cursor_to(&cx, &m, &["c", "a"]);

    let expr = parse_xpath("../b").unwrap();
    let nsc = ns("m", &[("m", "m")]);
    let deps = compile_dep_paths(&expr, &nsc, &cur);
    assert_eq!(deps.len(), 1);
    assert_eq!(
        deps[0],
        DepPath {
            absolute: false,
            steps: vec![
                DepStep::Parent,
                DepStep::Child { ns: "m".into(), name: "b".into() },
            ],
        }
    );
}

#[test]
fn sibling_through_transparent_case_does_not_become_unknown() {
    // `ref` is inside case `a`; `protocol` is a sibling of the choice. `../protocol`
    // must resolve through the transparent case+choice, not produce `<unknown>`.
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  container c {
    leaf protocol { type string; }
    choice ch {
      case a { leaf ref { type string; } }
    }
  }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let cur = cursor_to(&cx, &m, &["c", "ref"]);

    let expr = parse_xpath("../protocol").unwrap();
    let nsc = ns("m", &[("m", "m")]);
    let deps = compile_dep_paths(&expr, &nsc, &cur);
    assert_eq!(deps.len(), 1);
    // `..` skips the transparent case+choice to reach `c`, then `protocol`.
    assert_eq!(
        deps[0].steps,
        vec![
            DepStep::Parent,
            DepStep::Child { ns: "m".into(), name: "protocol".into() },
        ]
    );
}

#[test]
fn preserves_per_step_namespace_across_modules() {
    // m augments dep:root with a container that contains a leaf; an absolute path
    // crosses from dep's namespace into m's namespace, and each step keeps its own.
    let reg = registry_from(&[
        (
            "dep",
            r#"
module dep {
  namespace "urn:dep";
  prefix d;
  container root { }
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
  augment "/d:root" {
    container extra { leaf val { type string; } }
  }
}
"#,
        ),
    ]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let dep = reg.resolve_import("dep", None).unwrap();
    let cur = Cursor::root_of(&dep, &cx); // context at dep root

    // /d:root/m:extra/m:val — first step in dep, then into m's augment.
    let expr = parse_xpath("/d:root/m:extra/m:val").unwrap();
    let nsc = ns("m", &[("d", "dep"), ("m", "m")]);
    let deps = compile_dep_paths(&expr, &nsc, &cur);
    assert_eq!(deps.len(), 1, "got {deps:?}");
    assert!(is_path_absolute(&deps[0]));
    assert_eq!(
        deps[0].steps,
        vec![
            DepStep::Child { ns: "dep".into(), name: "root".into() },
            DepStep::Child { ns: "m".into(), name: "extra".into() },
            DepStep::Child { ns: "m".into(), name: "val".into() },
        ],
        "per-step namespaces must be preserved across the module boundary"
    );

    // Round-trips to cursor axes.
    let axes = dep_path_to_cursor_path(&deps[0]);
    assert_eq!(axes.len(), 3);
}

#[test]
fn extracts_paths_from_comparison_and_predicates() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  container c {
    leaf a { type string; }
    leaf b { type string; }
  }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let cur = cursor_to(&cx, &m, &["c", "a"]);

    // `../a = ../b` references two sibling paths.
    let expr = parse_xpath("../a = ../b").unwrap();
    let nsc = ns("m", &[("m", "m")]);
    let deps = compile_dep_paths(&expr, &nsc, &cur);
    assert_eq!(deps.len(), 2, "both operands contribute a dep path: {deps:?}");
    let names: Vec<&str> = deps
        .iter()
        .filter_map(|d| match d.steps.last() {
            Some(DepStep::Child { name, .. }) => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["a", "b"]);
}
