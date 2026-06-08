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
use crate::compiler::ExtensionInstance;
use crate::plugin::OverlayExtension;
use crate::types_registry::{ConstraintKind, TypeResolutionObserver};

/// Walk with a name-recording visitor; returns the names of all visited nodes
/// in encounter order (own-tree then own-augments).
fn visited_names(walker: &SchemaWalker<'_>) -> Vec<String> {
    let mut names = Vec::new();
    let mut obs = EventRecorder::default();
    walker.walk_with_visitor(&mut obs, |c| {
        if let Some(n) = c.current() {
            names.push(n.name.clone());
        }
    });
    names
}

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
    fn on_extension_attached(
        &mut self,
        source_module: &str,
        ext: &ExtensionInstance,
        _node: &SchemaNode,
    ) {
        self.events.push(format!("ext:{source_module}:{}", ext.name));
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

// ── Step 6: on_extension_attached (§15) ──────────────────────────────────────

#[test]
fn fires_on_extension_attached_for_foreign_module_only() {
    // Leaf `x` carries an own-module extension (`m:hint`) and a foreign-module
    // extension (`b:callpoint`). Only the foreign one fires.
    let reg = registry_from(&[
        (
            "b",
            r#"
module b {
  namespace "urn:b";
  prefix b;
  extension callpoint { argument n; }
}
"#,
        ),
        (
            "m",
            r#"
module m {
  namespace "urn:m";
  prefix m;
  import b { prefix b; }
  extension hint { argument n; }
  leaf x { type string; m:hint "h"; b:callpoint "cp"; }
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
    assert_eq!(obs.events, vec!["ext:b:callpoint".to_string()]);
}

#[test]
fn fires_extensions_in_declaration_order_no_dedup() {
    // Three foreign extensions in order p1:e1, p2:e2, p1:e3 — emitted in exactly
    // that order, with no per-source dedup at the walker layer.
    let reg = registry_from(&[
        (
            "p1",
            r#"
module p1 {
  namespace "urn:p1";
  prefix p1;
  extension e1 { argument n; }
  extension e3 { argument n; }
}
"#,
        ),
        (
            "p2",
            r#"
module p2 {
  namespace "urn:p2";
  prefix p2;
  extension e2 { argument n; }
}
"#,
        ),
        (
            "m",
            r#"
module m {
  namespace "urn:m";
  prefix m;
  import p1 { prefix p1; }
  import p2 { prefix p2; }
  leaf x { type string; p1:e1 "1"; p2:e2 "2"; p1:e3 "3"; }
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
    assert_eq!(
        obs.events,
        vec![
            "ext:p1:e1".to_string(),
            "ext:p2:e2".to_string(),
            "ext:p1:e3".to_string(),
        ]
    );
}

#[test]
fn default_observer_method_means_no_extension_events() {
    // An observer that does not override on_extension_attached sees nothing from
    // it (default no-op), even when foreign extensions are present.
    struct OnlyTypeObserver(Vec<String>);
    impl TypeResolutionObserver for OnlyTypeObserver {
        fn on_typedef_resolved(&mut self, src: &str, td: &Typedef) {
            self.0.push(format!("typedef:{src}:{}", td.name));
        }
        // on_extension_attached: default (no-op)
    }
    let reg = registry_from(&[
        (
            "b",
            r#"
module b {
  namespace "urn:b";
  prefix b;
  extension callpoint { argument n; }
}
"#,
        ),
        (
            "m",
            r#"
module m {
  namespace "urn:m";
  prefix m;
  import b { prefix b; }
  leaf x { type string; b:callpoint "cp"; }
}
"#,
        ),
    ]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let types = TypeRegistry::new(&reg);
    let walker = SchemaWalker::new(&m, &reg, &types, &cx);

    let mut obs = OnlyTypeObserver(Vec::new());
    walker.walk(&mut obs);
    assert!(obs.0.is_empty(), "default on_extension_attached records nothing");
}

#[test]
fn fires_constraint_then_extension_then_resolution() {
    // A leaf with a `when` (own module), a foreign extension (cc:callpoint), and
    // a leafref into module dd. The events at the node must be ordered:
    // constraint-source, then extension, then leafref resolution.
    let reg = registry_from(&[
        (
            "cc",
            r#"
module cc {
  namespace "urn:cc";
  prefix cc;
  extension callpoint { argument n; }
}
"#,
        ),
        (
            "dd",
            r#"
module dd {
  namespace "urn:dd";
  prefix dd;
  leaf target { type string; }
}
"#,
        ),
        (
            "m",
            r#"
module m {
  namespace "urn:m";
  prefix m;
  import cc { prefix cc; }
  import dd { prefix dd; }
  leaf x {
    type leafref { path "/dd:target"; }
    when "true()";
    cc:callpoint "cp";
  }
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
    assert_eq!(
        obs.events,
        vec![
            "when:m:x".to_string(),
            "ext:cc:callpoint".to_string(),
            "leafref:dd:target".to_string(),
        ]
    );
}

// ── Step 3: walk_own_augments (§11) ──────────────────────────────────────────

#[test]
fn own_augments_fire_in_declaration_order() {
    // M contributes two augments into host H (both into /h:root). Walking M
    // visits both augmented leaves, in source-declaration order.
    let reg = registry_from(&[
        (
            "h",
            r#"
module h { namespace "urn:h"; prefix h; container root { } }
"#,
        ),
        (
            "m",
            r#"
module m {
  namespace "urn:m";
  prefix m;
  import h { prefix h; }
  augment "/h:root" { leaf a { type string; } }
  augment "/h:root" { leaf b { type string; } }
}
"#,
        ),
    ]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let types = TypeRegistry::new(&reg);
    let walker = SchemaWalker::new(&m, &reg, &types, &cx);
    assert_eq!(visited_names(&walker), vec!["a", "b"]);
}

#[test]
fn own_augments_skipped_if_feature_disabled() {
    // M's augment is guarded by `if-feature myfeat`. With a strict feature set
    // that does not enable myfeat, the augment contributes no events.
    let reg = registry_from(&[
        (
            "h",
            r#"
module h { namespace "urn:h"; prefix h; container root { } }
"#,
        ),
        (
            "m",
            r#"
module m {
  namespace "urn:m";
  prefix m;
  import h { prefix h; }
  feature myfeat;
  augment "/h:root" {
    if-feature myfeat;
    leaf a { type string; }
  }
}
"#,
        ),
    ]);
    // Strict whitelist: m is "listed" (one enabled feature) but myfeat is not,
    // so myfeat evaluates disabled.
    let mut feats = HashSet::new();
    feats.insert(("m".to_string(), "other".to_string()));
    let cx = ExpansionCtx::new(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let types = TypeRegistry::new(&reg);
    let walker = SchemaWalker::new(&m, &reg, &types, &cx);
    assert!(visited_names(&walker).is_empty(), "disabled augment fires nothing");
}

#[test]
fn own_augments_position_cursor_at_target() {
    // A leafref in the augment body resolving via `..` must find a sibling in the
    // HOST tree (the cursor is positioned at the augment target in the host).
    let reg = registry_from(&[
        (
            "h",
            r#"
module h {
  namespace "urn:h";
  prefix h;
  container root { leaf hostleaf { type string; } }
}
"#,
        ),
        (
            "m",
            r#"
module m {
  namespace "urn:m";
  prefix m;
  import h { prefix h; }
  augment "/h:root" {
    leaf ref { type leafref { path "../h:hostleaf"; } }
  }
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
    // `../hostleaf` resolves to the host's own leaf.
    assert_eq!(obs.events, vec!["leafref:h:hostleaf".to_string()]);
}

#[test]
fn cross_module_augment_chain_per_module_attribution() {
    // A augments B, B augments C. Each module's walk fires only its own tree
    // plus its own augments — never another module's augment.
    let reg = registry_from(&[
        (
            "c",
            r#"
module c { namespace "urn:c"; prefix c; container root { } }
"#,
        ),
        (
            "b",
            r#"
module b {
  namespace "urn:b";
  prefix b;
  import c { prefix c; }
  container broot { }
  augment "/c:root" { leaf bx { type string; } }
}
"#,
        ),
        (
            "a",
            r#"
module a {
  namespace "urn:a";
  prefix a;
  import b { prefix b; }
  augment "/b:broot" { leaf ax { type string; } }
}
"#,
        ),
    ]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let types = TypeRegistry::new(&reg);

    let a = reg.resolve_import("a", None).unwrap();
    let b = reg.resolve_import("b", None).unwrap();
    let c = reg.resolve_import("c", None).unwrap();

    assert_eq!(visited_names(&SchemaWalker::new(&a, &reg, &types, &cx)), vec!["ax"]);
    assert_eq!(
        visited_names(&SchemaWalker::new(&b, &reg, &types, &cx)),
        vec!["broot", "bx"]
    );
    assert_eq!(visited_names(&SchemaWalker::new(&c, &reg, &types, &cx)), vec!["root"]);
}

#[test]
fn walk_calls_own_tree_then_own_augments() {
    // walk() event sequence == walk_own_tree() events ++ walk_own_augments() events.
    let reg = registry_from(&[
        (
            "h",
            r#"
module h { namespace "urn:h"; prefix h; container root { } }
"#,
        ),
        (
            "m",
            r#"
module m {
  namespace "urn:m";
  prefix m;
  import h { prefix h; }
  typedef t1 { type string; }
  typedef t2 { type string; }
  leaf own { type t1; }
  augment "/h:root" { leaf aug { type t2; } }
}
"#,
        ),
    ]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let types = TypeRegistry::new(&reg);
    let walker = SchemaWalker::new(&m, &reg, &types, &cx);

    let mut all = EventRecorder::default();
    walker.walk(&mut all);
    let mut tree = EventRecorder::default();
    walker.walk_own_tree(&mut tree);
    let mut augs = EventRecorder::default();
    walker.walk_own_augments(&mut augs);

    assert_eq!(tree.events, vec!["typedef:m:t1".to_string()]);
    assert_eq!(augs.events, vec!["typedef:m:t2".to_string()]);
    let mut combined = tree.events.clone();
    combined.extend(augs.events.clone());
    assert_eq!(all.events, combined);
}

#[test]
fn own_augments_surface_host_applied_annotations() {
    // M augments host H at /h:root with leaf x. An annotation module injects a
    // foreign extension (xann:callpoint) onto /h:root/m:x. Walking M must fire
    // on_extension_attached for that foreign extension on x — proving the walk
    // surfaces extensions reached through the augment splice point.
    let parse = |src: &str| {
        let (stmts, errs) = parse_yang(src, Arc::from("t.yang"));
        assert!(errs.is_empty(), "parse errors: {errs:?}");
        let stmt = stmts.into_iter().next().unwrap();
        let name = stmt.arg.clone().unwrap();
        (ModuleKey::latest(&name), stmt)
    };

    let h_src = r#"
module h { namespace "urn:h"; prefix h; container root { } }
"#;
    let m_src = r#"
module m {
  namespace "urn:m";
  prefix m;
  import h { prefix h; }
  augment "/h:root" { leaf x { type string; } }
}
"#;
    // Annotation module: uses the overlay extension `xann:annotate` whose body
    // carries the extension to inject onto the target node.
    let ann_src = r#"
module annmod {
  namespace "urn:annmod";
  prefix am;
  import h { prefix h; }
  import m { prefix m; }
  import xann { prefix xann; }
  xann:annotate "/h:root/m:x" {
    xann:callpoint "cp";
  }
}
"#;

    let (h_key, h_stmt) = parse(h_src);
    let (m_key, m_stmt) = parse(m_src);
    let (ann_key, ann_stmt) = parse(ann_src);

    // Build an annotation index that recognises `xann:annotate` as an overlay.
    static OVERLAY: &[OverlayExtension] = &[OverlayExtension {
        module: "xann",
        name: "annotate",
        source_plugin: "test",
    }];
    let ann_index = AnnotationIndex::build(&[(ann_key, ann_stmt)], OVERLAY);

    let mut reg = ModuleRegistry::new();
    for (key, stmt) in [(h_key, h_stmt), (m_key, m_stmt)] {
        let compiled = compile_module(
            &key,
            stmt,
            &reg,
            &DeviationIndex::default(),
            &ann_index,
            &AstAnnotationIndex::default(),
        );
        reg.insert(Arc::new(compiled));
    }

    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let types = TypeRegistry::new(&reg);
    let walker = SchemaWalker::new(&m, &reg, &types, &cx);

    let mut obs = EventRecorder::default();
    walker.walk(&mut obs);
    // The event reports the *injection-source* module (the annotation module
    // `annmod`), not the extension's defining module (`xann`) — follow-up B.
    assert_eq!(
        obs.events,
        vec!["ext:annmod:callpoint".to_string()],
        "annotation-injected foreign extension on the augmented node is surfaced"
    );
}

// ── Follow-up B: injection_source_module (§14) ───────────────────────────────

#[test]
fn source_for_ns_prefers_injection_source() {
    use crate::ast::Pos;
    let pos = Pos::FilePos { file: Arc::from("t"), line: 1 };
    let direct = ExtensionInstance {
        module: "defmod".into(),
        name: "callpoint".into(),
        arg: None,
        substmts: vec![],
        pos: pos.clone(),
        injection_source_module: None,
    };
    assert_eq!(direct.source_for_ns(), "defmod");

    let injected = ExtensionInstance {
        module: "defmod".into(),
        name: "callpoint".into(),
        arg: None,
        substmts: vec![],
        pos,
        injection_source_module: Some("ann-mod".into()),
    };
    assert_eq!(injected.source_for_ns(), "ann-mod");
}

#[test]
fn path_annotation_reports_injection_source_module() {
    // An annotation module injects a foreign extension onto a plain (non-augmented)
    // node. on_extension_attached must report the annotation module, not the
    // extension's defining module.
    let parse = |src: &str| {
        let (stmts, errs) = parse_yang(src, Arc::from("t.yang"));
        assert!(errs.is_empty(), "parse errors: {errs:?}");
        let stmt = stmts.into_iter().next().unwrap();
        let name = stmt.arg.clone().unwrap();
        (ModuleKey::latest(&name), stmt)
    };

    let m_src = r#"
module m { namespace "urn:m"; prefix m; leaf x { type string; } }
"#;
    let ann_src = r#"
module annmod {
  namespace "urn:annmod";
  prefix am;
  import m { prefix m; }
  import xann { prefix xann; }
  xann:annotate "/m:x" { xann:callpoint "cp"; }
}
"#;
    let (m_key, m_stmt) = parse(m_src);
    let (ann_key, ann_stmt) = parse(ann_src);

    static OVERLAY: &[OverlayExtension] = &[OverlayExtension {
        module: "xann",
        name: "annotate",
        source_plugin: "test",
    }];
    let ann_index = AnnotationIndex::build(&[(ann_key, ann_stmt)], OVERLAY);

    let mut reg = ModuleRegistry::new();
    let compiled = compile_module(
        &m_key,
        m_stmt,
        &reg,
        &DeviationIndex::default(),
        &ann_index,
        &AstAnnotationIndex::default(),
    );
    reg.insert(Arc::new(compiled));

    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let types = TypeRegistry::new(&reg);
    let walker = SchemaWalker::new(&m, &reg, &types, &cx);

    let mut obs = EventRecorder::default();
    walker.walk(&mut obs);
    assert_eq!(obs.events, vec!["ext:annmod:callpoint".to_string()]);
}

// ── Uses-body augment (§11 follow-up A, via cached expansion + memoized cursor)

#[test]
fn own_augments_with_uses_body_visit_expanded_leaves() {
    let reg = registry_from(&[
        ("h", r#"module h { namespace "urn:h"; prefix h; container root { } }"#),
        (
            "m",
            r#"
module m {
  namespace "urn:m";
  prefix m;
  import h { prefix h; }
  grouping g { leaf ga { type string; } leaf gb { type string; } }
  augment "/h:root" { uses g; }
}
"#,
        ),
    ]);
    let feats = HashSet::new();
    let cx = ctx(&reg, &feats);
    let m = reg.resolve_import("m", None).unwrap();
    let types = TypeRegistry::new(&reg);
    let walker = SchemaWalker::new(&m, &reg, &types, &cx);
    assert_eq!(
        visited_names(&walker),
        vec!["ga", "gb"],
        "uses-shaped augment body must surface the grouping's expanded leaves"
    );
}
