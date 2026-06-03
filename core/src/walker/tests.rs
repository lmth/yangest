// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
use std::collections::HashSet;
use std::sync::Arc;

use super::*;
use crate::annindex::AnnotationIndex;
use crate::astannindex::AstAnnotationIndex;
use crate::ast::ModuleKey;
use crate::compiler::{compile_module, ModuleRegistry, Typedef};
use crate::devindex::DeviationIndex;
use crate::parser::parse_yang;
use crate::types_registry::{ConstraintKind, TypeResolutionObserver};

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

/// Records the sequence of resolution events as `"kind:source:name"` strings.
#[derive(Default)]
struct EventRecorder {
    events: Vec<String>,
}

impl TypeResolutionObserver for EventRecorder {
    fn on_typedef_resolved(&mut self, source_module: &str, typedef: &Typedef) {
        self.events.push(format!("typedef:{source_module}:{}", typedef.name));
    }
    fn on_leafref_resolved(&mut self, _source_module: &str, target: &SchemaNode) {
        // Match the design's consumer pattern (§8): the namespace that matters is
        // the *target* node's module, read from the target itself.
        self.events.push(format!("leafref:{}:{}", target.module_name, target.name));
    }
    fn on_constraint_source(
        &mut self,
        source_module: &str,
        kind: ConstraintKind,
        node: &SchemaNode,
    ) {
        let k = match kind {
            ConstraintKind::When => "when",
            ConstraintKind::Must => "must",
        };
        self.events.push(format!("{k}:{source_module}:{}", node.name));
    }
}

#[test]
fn walks_children_in_declaration_order() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  leaf first { type string; }
  container c {
    leaf inner { type string; }
    leaf inner2 { type string; }
  }
  leaf last { type string; }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let types = TypeRegistry::new(&reg);
    let walker = SchemaWalker::new(&m, &reg, &types, &cx);

    let mut order = Vec::new();
    let mut obs = EventRecorder::default();
    walker.walk_with_visitor(&mut obs, |cur| {
        if let Some(n) = cur.current() {
            order.push(n.name.clone());
        }
    });
    assert_eq!(order, vec!["first", "c", "inner", "inner2", "last"]);
}

#[test]
fn fires_typedef_events_before_descending() {
    // Type is resolved *before* descending into children: the container's leaf
    // type events come after the container is visited, and a top-level leaf's
    // typedef chain fires in resolve order.
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
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let types = TypeRegistry::new(&reg);
    let walker = SchemaWalker::new(&m, &reg, &types, &cx);

    let mut obs = EventRecorder::default();
    walker.walk(&mut obs);
    assert_eq!(
        obs.events,
        vec!["typedef:m:mid-str".to_string(), "typedef:m:base-str".to_string()]
    );
}

#[test]
fn fires_union_member_events_in_declaration_order() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  typedef a-t { type uint8; }
  typedef b-t { type uint16; }
  leaf u { type union { type a-t; type b-t; } }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let types = TypeRegistry::new(&reg);
    let walker = SchemaWalker::new(&m, &reg, &types, &cx);

    let mut obs = EventRecorder::default();
    walker.walk(&mut obs);
    assert_eq!(
        obs.events,
        vec!["typedef:m:a-t".to_string(), "typedef:m:b-t".to_string()]
    );
}

#[test]
fn fires_leafref_event_with_target_module() {
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
    let types = TypeRegistry::new(&reg);
    let walker = SchemaWalker::new(&m, &reg, &types, &cx);

    let mut obs = EventRecorder::default();
    walker.walk(&mut obs);
    // The leafref's target lives in `dep`; the event reports the target module.
    assert_eq!(obs.events, vec!["leafref:dep:id".to_string()]);
}

#[test]
fn leafref_inside_case_resolves_sibling_through_transparent_choice() {
    // A leafref inside a `case` referencing a sibling of the choice must resolve
    // (choice/case transparent), firing the leafref event.
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
    let types = TypeRegistry::new(&reg);
    let walker = SchemaWalker::new(&m, &reg, &types, &cx);

    let mut obs = EventRecorder::default();
    walker.walk(&mut obs);
    assert_eq!(obs.events, vec!["leafref:m:protocol".to_string()]);
}

#[test]
fn noop_walk_does_not_panic_and_visits_all_nodes() {
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  list l {
    key "k";
    leaf k { type string; }
    leaf v { type string; }
  }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let types = TypeRegistry::new(&reg);
    let walker = SchemaWalker::new(&m, &reg, &types, &cx);

    let mut count = 0usize;
    let mut obs = EventRecorder::default();
    walker.walk_with_visitor(&mut obs, |_| count += 1);
    assert_eq!(count, 3, "list + its two leaves");
    assert!(walker.options().follow_deviations);
    assert_eq!(walker.module().key.name, "m");
    assert_eq!(walker.registry().modules.len(), 1);
}

#[test]
fn fires_constraint_source_for_when_and_must_in_declaration_order() {
    // A leaf with a `when` (original) and a container with a `must` (original):
    // both should fire on_constraint_source with the node's own module as the
    // source. `when` fires before `must` at the same node (for nodes that
    // carry both the spec is when-then-must — covered by a separate test).
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  container c {
    must "true()";
    leaf l {
      when "true()";
      type string;
    }
  }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let types = TypeRegistry::new(&reg);
    let walker = SchemaWalker::new(&m, &reg, &types, &cx);

    let mut obs = EventRecorder::default();
    walker.walk(&mut obs);

    // Expected: visit container c → must fires; visit leaf l → when fires;
    // type resolution fires nothing for plain `string`.
    assert_eq!(
        obs.events,
        vec!["must:m:c", "when:m:l"],
        "constraint events should fire in walk encounter order"
    );
}

#[test]
fn fires_when_then_must_at_same_node() {
    // A node that carries BOTH when and must: the walker fires when first,
    // then must, both with the node's own module.
    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  leaf l {
    when "true()";
    must "true()";
    type string;
  }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let types = TypeRegistry::new(&reg);
    let walker = SchemaWalker::new(&m, &reg, &types, &cx);

    let mut obs = EventRecorder::default();
    walker.walk(&mut obs);

    assert_eq!(obs.events, vec!["when:m:l", "must:m:l"]);
}

#[test]
fn default_observer_method_means_no_constraint_events() {
    // An observer that does NOT override on_constraint_source must not see
    // any constraint events, only typedef/leafref events. Confirms the
    // backward-compat default-method extension property documented in §12.
    struct OnlyTypeObserver(Vec<String>);
    impl TypeResolutionObserver for OnlyTypeObserver {
        fn on_typedef_resolved(&mut self, src: &str, td: &Typedef) {
            self.0.push(format!("typedef:{src}:{}", td.name));
        }
        // on_leafref_resolved + on_constraint_source: defaults (no-op)
    }

    let reg = registry_from(&[(
        "m",
        r#"
module m {
  namespace "urn:m";
  prefix m;
  leaf l {
    when "true()";
    must "true()";
    type string;
  }
}
"#,
    )]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let types = TypeRegistry::new(&reg);
    let walker = SchemaWalker::new(&m, &reg, &types, &cx);

    let mut obs = OnlyTypeObserver(Vec::new());
    walker.walk(&mut obs);
    assert!(obs.0.is_empty(), "no events for plain string + default observer methods");
}
