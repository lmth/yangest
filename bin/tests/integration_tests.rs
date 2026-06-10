// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
/// Integration tests for the yangest YANG compiler.
///
/// Each test runs the yangest binary (via `CARGO_BIN_EXE_yangest`) on test YANG
/// files located in `tests/yang/` and checks that the output matches the
/// expected reference output.
use std::path::PathBuf;
use std::process::Command;

// ── helpers ─────────────────────────────────────────────────────────────────

fn yangest_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_yangest"))
}

fn yang_dir(subdir: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/yang")
        .join(subdir)
}

/// Run yangest with the given args. Args ending with `.yang` are resolved
/// relative to `tests/yang/<base_dir>`; all other args are passed verbatim.
/// Returns (stdout, stderr, exit_code).
fn run(base_dir: &str, args: &[&str]) -> (String, String, i32) {
    let dir = yang_dir(base_dir);
    let mut cmd = Command::new(yangest_bin());
    for arg in args {
        if arg.ends_with(".yang") {
            cmd.arg(dir.join(arg));
        } else {
            cmd.arg(arg);
        }
    }
    let out = cmd.output().expect("failed to run yangest");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// Assert `stdout` contains every line in `expected_lines`, in order (not
/// necessarily contiguous).
fn assert_contains_in_order(stdout: &str, expected_lines: &[&str]) {
    let lines: Vec<&str> = stdout.lines().collect();
    let mut pos = 0;
    for expected in expected_lines {
        let found = lines[pos..].iter().position(|l| l.contains(expected));
        assert!(
            found.is_some(),
            "expected line {:?} not found in output after line {pos}.\nFull output:\n{stdout}",
            expected
        );
        pos += found.unwrap() + 1;
    }
}

// ── groupings ────────────────────────────────────────────────────────────────

/// Nested groupings: AREA-TABLE → AREA-CONTENT → EXTERNAL-OUT
/// The leaf should appear directly under /ospf after full expansion.
#[test]
fn nested_groupings_expanded_correctly() {
    let (stdout, _stderr, code) = run("groupings", &["-f", "tree", "a.yang"]);
    assert_eq!(code, 0, "unexpected exit code\nstdout: {stdout}");
    assert_contains_in_order(
        &stdout,
        &["module: a", "+--rw ospf", "+--rw external-out?   boolean"],
    );
}

/// Uses-in-augment: a grouping is used inside an augment statement.
/// The augmented nodes must appear in the container tree after expansion.
#[test]
fn uses_inside_augment() {
    let (stdout, _stderr, code) = run("groupings", &["-f", "tree", "b.yang"]);
    assert_eq!(code, 0, "unexpected exit code\nstdout: {stdout}");
    assert_contains_in_order(
        &stdout,
        &["module: b", "+--rw q", "+--rw xx", "+--rw uu", "+--rw vv"],
    );
}

// ── refine ───────────────────────────────────────────────────────────────────

/// Refine: presence added, mandatory→optional, config false, min/max elements
/// set, default changed, must added.  Checked via the tree plugin.
#[test]
fn refine_tree_output() {
    let (stdout, _stderr, code) = run("refine", &["-f", "tree", "ref.yang"]);
    assert_eq!(code, 0, "unexpected exit code\nstdout: {stdout}");
    assert_contains_in_order(
        &stdout,
        &[
            "module: ref",
            "+--rw a!",
            "+--rw b?   string",
            "+--ro c?   string",
            "+--rw d*   string",
        ],
    );
}

// ── deviations ───────────────────────────────────────────────────────────────

/// Cross-module deviation: module a deviates module b.
///   - b:x/r must become optional (mandatory false) with type uint32
///   - asub's deviation on b:x/u must remove u from module b
#[test]
fn cross_module_deviation_removes_leaf_and_changes_mandatory() {
    let (stdout, stderr, code) = run(
        "deviations",
        &["-f", "tree", "b.yang", "a.yang", "asub.yang"],
    );
    assert_eq!(
        code, 0,
        "unexpected exit code\nstdout: {stdout}\nstderr: {stderr}"
    );

    // module b: u removed by asub, r optional
    assert_contains_in_order(&stdout, &["module: b", "+--ro x", "+--ro r?   uint32"]);
    // module a: bar list with baz removed by self-deviation
    assert_contains_in_order(
        &stdout,
        &[
            "module: a",
            "+--ro x",
            "+--ro bar* [foo]",
            "+--ro foo    int32",
            "+--ro bar    int32",
        ],
    );

    // baz must NOT appear in module a's output
    let a_section_start = stdout.find("module: a").expect("module: a not found");
    let b_section_start = stdout.find("module: b").expect("module: b not found");
    let a_section = &stdout[a_section_start..b_section_start.min(stdout.len())];
    assert!(
        !a_section.contains("baz"),
        "baz should have been removed by deviation\nmodule a section:\n{a_section}"
    );
}

/// Self-augment + self-deviation: module a augments its own /x with a list,
/// then deviates a leaf inside that list (not-supported).
#[test]
fn self_augment_node_targeted_by_deviation() {
    let (stdout, stderr, code) = run(
        "deviations",
        &["-f", "tree", "a.yang", "b.yang", "asub.yang"],
    );
    assert_eq!(
        code, 0,
        "unexpected exit code\nstdout: {stdout}\nstderr: {stderr}"
    );

    // baz should be deviated away
    let a_start = stdout.find("module: a").expect("module: a not found");
    let a_section_end = stdout[a_start..]
        .find("module: b")
        .map(|p| a_start + p)
        .unwrap_or(stdout.len());
    let a_section = &stdout[a_start..a_section_end];
    assert!(
        !a_section.contains("baz"),
        "baz should have been removed by deviation\nmodule a output:\n{a_section}"
    );
}

/// Cross-module deviation: add/delete must statements on simple.yang via dev.yang.
/// Note: simple.yang imports ietf-inet-types which is not provided — we only
/// validate that the deviation itself doesn't cause a crash and that the tree
/// is rendered.
#[test]
fn deviation_add_delete_must() {
    let (stdout, _stderr, _code) = run("deviations", &["-f", "tree", "simple.yang", "dev.yang"]);
    // The module should still appear even with unresolved imports
    assert!(
        stdout.contains("module: simple"),
        "expected module simple in output\n{stdout}"
    );
    assert!(
        stdout.contains("+--rw servers"),
        "expected servers container\n{stdout}"
    );
}

/// Deviation not-supported removes a list leaf.
#[test]
fn deviation_not_supported_on_list() {
    let (stdout, _stderr, _code) = run("deviations", &["-f", "tree", "list.yang", "list-dev.yang"]);
    // foobar should still be present; baz should be deviated away
    assert!(
        stdout.contains("foobar"),
        "foobar should still be present\n{stdout}"
    );
    // The baz child leaf should NOT appear as its own tree node line.
    // (The key spec in the list header may still show `[bar baz]` — that is
    // a rendering artifact we do not validate here.)
    let has_baz_leaf = stdout.lines().any(|l| {
        l.contains("+--") && (l.ends_with("baz") || l.contains("baz ") || l.contains("baz\t"))
    });
    assert!(
        !has_baz_leaf,
        "baz leaf node should have been deviated away\n{stdout}"
    );
}

// ── tree plugin ───────────────────────────────────────────────────────────────

/// Cross-module augments appear as "augment /module:node:" sections.
#[test]
fn tree_shows_augment_section_for_foreign_augment() {
    let (stdout, _stderr, code) = run("tree", &["-f", "tree", "small6.yang", "parent.yang"]);
    assert_eq!(code, 0, "unexpected exit code\nstdout: {stdout}");
    assert_contains_in_order(
        &stdout,
        &[
            "augment /parent:parent-container:",
            "+--rw test-list* [test-key]",
            "+--rw test-key    string",
        ],
    );
}

/// Both augment sections are rendered when a module augments two different targets.
#[test]
fn tree_multiple_augment_sections() {
    let (stdout, _stderr, code) = run("tree", &["-f", "tree", "small6.yang", "parent.yang"]);
    assert_eq!(code, 0, "unexpected exit code\nstdout: {stdout}");
    assert!(
        stdout.contains("augment /parent:parent-container:"),
        "first augment section missing\n{stdout}"
    );
    assert!(
        stdout.contains("augment /parent:parent-container-2:"),
        "second augment section missing\n{stdout}"
    );
}

// ── anyxml / anydata + must ───────────────────────────────────────────────────

/// YANG 1.1: must is a valid sub-statement of anyxml and anydata.
#[test]
fn anyxml_anydata_with_must_compiles_cleanly() {
    let (stdout, stderr, code) = run("any_must", &["-f", "tree", "any.yang"]);
    assert_eq!(
        code, 0,
        "unexpected exit code\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.to_lowercase().contains("error"),
        "unexpected errors in stderr:\n{stderr}"
    );
    assert!(
        stdout.contains("module: any"),
        "expected module: any in output\n{stdout}"
    );
}

// ── depend plugin ─────────────────────────────────────────────────────────────

const DEPEND_ALL: &[&str] = &[
    "a.yang", "b.yang", "c.yang", "d.yang", "e.yang", "f.yang", "g.yang",
];

/// Basic depend output: direct imports + includes, declaration order.
#[test]
fn depend_basic_direct_deps() {
    let mut args = vec!["-f", "depend"];
    args.extend_from_slice(DEPEND_ALL);
    let (stdout, _stderr, code) = run("depend", &args);
    assert_eq!(code, 0, "unexpected exit code\nstdout: {stdout}");
    // Only check the a.yang line
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("a.yang"))
        .expect("no a.yang line in output");
    let rhs = line.split(':').nth(1).unwrap_or("").trim().to_string();
    assert_eq!(rhs, "b c d f", "unexpected deps: {line}");
}

/// --depend-no-submodules: includes (submodules) are excluded.
#[test]
fn depend_no_submodules() {
    let mut args = vec!["-f", "depend", "--depend-no-submodules"];
    args.extend_from_slice(DEPEND_ALL);
    let (stdout, _stderr, code) = run("depend", &args);
    assert_eq!(code, 0, "unexpected exit code\nstdout: {stdout}");
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("a.yang"))
        .expect("no a.yang line");
    let rhs = line.split(':').nth(1).unwrap_or("").trim().to_string();
    assert_eq!(rhs, "b c", "unexpected deps: {line}");
}

/// --depend-recurse: transitive deps, sorted.
#[test]
fn depend_recurse() {
    let mut args = vec!["-f", "depend", "--depend-recurse"];
    args.extend_from_slice(DEPEND_ALL);
    let (stdout, _stderr, code) = run("depend", &args);
    assert_eq!(code, 0, "unexpected exit code\nstdout: {stdout}");
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("a.yang"))
        .expect("no a.yang line");
    let rhs = line.split(':').nth(1).unwrap_or("").trim().to_string();
    assert_eq!(rhs, "b c d e f g", "unexpected deps: {line}");
}

/// --depend-no-submodules --depend-recurse: transitive imports only.
#[test]
fn depend_no_submodules_recurse() {
    let mut args = vec!["-f", "depend", "--depend-no-submodules", "--depend-recurse"];
    args.extend_from_slice(DEPEND_ALL);
    let (stdout, _stderr, code) = run("depend", &args);
    assert_eq!(code, 0, "unexpected exit code\nstdout: {stdout}");
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("a.yang"))
        .expect("no a.yang line");
    let rhs = line.split(':').nth(1).unwrap_or("").trim().to_string();
    assert_eq!(rhs, "b c e", "unexpected deps: {line}");
}

/// --depend-recurse --depend-ignore-module: filtered module is excluded from
/// output but its transitive deps are still included.
#[test]
fn depend_recurse_ignore_module() {
    let mut args = vec![
        "-f",
        "depend",
        "--depend-recurse",
        "--depend-ignore-module",
        "b",
    ];
    args.extend_from_slice(DEPEND_ALL);
    let (stdout, _stderr, code) = run("depend", &args);
    assert_eq!(code, 0, "unexpected exit code\nstdout: {stdout}");
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("a.yang"))
        .expect("no a.yang line");
    let rhs = line.split(':').nth(1).unwrap_or("").trim().to_string();
    assert_eq!(rhs, "c d e f g", "unexpected deps: {line}");
}

/// --depend-recurse --depend-extension: custom extension replaces .yang.
#[test]
fn depend_recurse_extension() {
    let mut args = vec![
        "-f",
        "depend",
        "--depend-recurse",
        "--depend-extension",
        ".test",
    ];
    args.extend_from_slice(DEPEND_ALL);
    let (stdout, _stderr, code) = run("depend", &args);
    assert_eq!(code, 0, "unexpected exit code\nstdout: {stdout}");
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("a.yang"))
        .expect("no a.yang line");
    let rhs = line.split(':').nth(1).unwrap_or("").trim().to_string();
    assert_eq!(
        rhs, "b.test c.test d.test e.test f.test g.test",
        "unexpected deps: {line}"
    );
}

/// --depend-recurse --depend-include-path --depend-double-colon: full paths
/// with :: separator.
#[test]
fn depend_recurse_include_path_double_colon() {
    let mut args = vec![
        "-f",
        "depend",
        "--depend-recurse",
        "--depend-include-path",
        "--depend-double-colon",
    ];
    args.extend_from_slice(DEPEND_ALL);
    let (stdout, _stderr, code) = run("depend", &args);
    assert_eq!(code, 0, "unexpected exit code\nstdout: {stdout}");
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("a.yang"))
        .expect("no a.yang line");
    assert!(line.contains("::"), "expected :: separator: {line}");
    // All six recursive deps should appear with .yang extension
    for dep in &["b.yang", "c.yang", "d.yang", "e.yang", "f.yang", "g.yang"] {
        assert!(line.contains(dep), "dep {dep} missing from: {line}");
    }
}

/// --depend-recurse --depend-include-path --depend-extension: extension wins
/// over include-path's default .yang.
#[test]
fn depend_recurse_include_path_extension() {
    let mut args = vec![
        "-f",
        "depend",
        "--depend-recurse",
        "--depend-include-path",
        "--depend-extension",
        ".test",
    ];
    args.extend_from_slice(DEPEND_ALL);
    let (stdout, _stderr, code) = run("depend", &args);
    assert_eq!(code, 0, "unexpected exit code\nstdout: {stdout}");
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("a.yang"))
        .expect("no a.yang line");
    let rhs = line.split(':').nth(1).unwrap_or("").trim().to_string();
    assert_eq!(
        rhs, "b.test c.test d.test e.test f.test g.test",
        "unexpected deps: {line}"
    );
}

/// --depend-target: custom makefile target name replaces the source file name.
/// Only a.yang is passed; imports are unresolved but the depend output is still
/// correct from the raw declared imports.
#[test]
fn depend_custom_target() {
    let (stdout, _stderr, _code) = run(
        "depend",
        &["-f", "depend", "--depend-target", "test_target", "a.yang"],
    );
    let line = stdout
        .lines()
        .find(|l| l.contains("test_target"))
        .expect("no test_target line");
    let rhs = line.split(':').nth(1).unwrap_or("").trim().to_string();
    assert_eq!(rhs, "b c d f", "unexpected deps: {line}");
}

// ── snapshot helpers ──────────────────────────────────────────────────────
//
// Snapshot tests compare the full stdout byte-for-byte against a stored
// `.expected` file.  They serve as the regression baseline for the lazy
// grouping rearchitecting: any change in output is immediately visible.
//
// To regenerate all snapshots after an intentional output change, run:
//   UPDATE_SNAPSHOTS=1 cargo test
//
// The helper writes the current output when that env-var is set; otherwise
// it asserts equality.

fn snapshot(base_dir: &str, args: &[&str], expected_file: &str) {
    let (stdout, stderr, code) = run(base_dir, args);
    let expected_path = yang_dir(base_dir).join(expected_file);

    if std::env::var("UPDATE_SNAPSHOTS").is_ok() {
        std::fs::write(&expected_path, &stdout).unwrap_or_else(|e| {
            panic!("failed to write snapshot {}: {e}", expected_path.display())
        });
        return;
    }

    let expected = std::fs::read_to_string(&expected_path).unwrap_or_else(|_| {
        panic!(
            "snapshot file not found: {}\nRun UPDATE_SNAPSHOTS=1 cargo test to create it.\nActual output:\n{stdout}",
            expected_path.display()
        )
    });

    assert_eq!(
        stdout, expected,
        "output differs from snapshot {expected_file}\n\
         Run UPDATE_SNAPSHOTS=1 cargo test to update.\n\
         stderr: {stderr}\nexit code: {code}"
    );
}

// ── tree snapshots — existing fixtures ────────────────────────────────────

#[test]
fn snapshot_groupings_a_tree() {
    snapshot("groupings", &["-f", "tree", "a.yang"], "a.tree.expected");
}

#[test]
fn snapshot_groupings_b_tree() {
    snapshot("groupings", &["-f", "tree", "b.yang"], "b.tree.expected");
}

#[test]
fn snapshot_refine_tree() {
    snapshot("refine", &["-f", "tree", "ref.yang"], "ref.tree.expected");
}

#[test]
fn snapshot_deviations_b_a_asub_tree() {
    snapshot(
        "deviations",
        &["-f", "tree", "b.yang", "a.yang", "asub.yang"],
        "b_a_asub.tree.expected",
    );
}

#[test]
fn snapshot_deviations_list_tree() {
    snapshot(
        "deviations",
        &["-f", "tree", "list.yang", "list-dev.yang"],
        "list_listdev.tree.expected",
    );
}

/// simple.yang uses ietf-inet-types which is not in the search path; yangest
/// still produces partial output.  The snapshot captures exactly what we emit
/// so any change (e.g. error message format, partial tree) is caught.
#[test]
fn snapshot_deviations_simple_dev_tree() {
    snapshot(
        "deviations",
        &["-f", "tree", "simple.yang", "dev.yang"],
        "simple_dev.tree.expected",
    );
}

#[test]
fn snapshot_tree_small6_parent() {
    snapshot(
        "tree",
        &["-f", "tree", "small6.yang", "parent.yang"],
        "small6_parent.tree.expected",
    );
}

#[test]
fn snapshot_any_must_tree() {
    snapshot("any_must", &["-f", "tree", "any.yang"], "any.tree.expected");
}

// ── yang-format snapshots ─────────────────────────────────────────────────

#[test]
fn snapshot_groupings_a_yang() {
    snapshot("groupings", &["-f", "yang", "a.yang"], "a.yang.expected");
}

#[test]
fn snapshot_refine_yang() {
    snapshot("refine", &["-f", "yang", "ref.yang"], "ref.yang.expected");
}

#[test]
fn snapshot_deviations_b_yang() {
    snapshot("deviations", &["-f", "yang", "b.yang"], "b.yang.expected");
}

/// Multi-module yang: submodule + two primary modules in one invocation.
#[test]
fn snapshot_deviations_b_a_asub_yang() {
    snapshot(
        "deviations",
        &["-f", "yang", "b.yang", "a.yang", "asub.yang"],
        "b_a_asub.yang.expected",
    );
}

// ── yang-expanded snapshots ───────────────────────────────────────────────

/// Single module: groupings fully inlined, deviations applied.
#[test]
fn snapshot_groupings_a_yang_expanded() {
    snapshot(
        "groupings",
        &["-f", "yang-expanded", "a.yang"],
        "a.yang-expanded.expected",
    );
}

/// Multi-module yang-expanded: deviations applied across all three modules.
#[test]
fn snapshot_deviations_b_a_asub_yang_expanded() {
    snapshot(
        "deviations",
        &["-f", "yang-expanded", "b.yang", "a.yang", "asub.yang"],
        "b_a_asub.yang-expanded.expected",
    );
}

// ── yin snapshots ─────────────────────────────────────────────────────────

/// Single module emitted as YIN XML.
#[test]
fn snapshot_groupings_a_yin() {
    snapshot("groupings", &["-f", "yin", "a.yang"], "a.yin.expected");
}

/// Multi-module yin: three modules in one invocation produce one XML stream.
#[test]
fn snapshot_deviations_b_a_asub_yin() {
    snapshot(
        "deviations",
        &["-f", "yin", "b.yang", "a.yang", "asub.yang"],
        "b_a_asub.yin.expected",
    );
}

// ── new fixtures: if-feature ──────────────────────────────────────────────

/// All features enabled (current default — no --feature flag filtering yet).
/// Every if-feature-gated node must appear in the tree.
/// This snapshot is the "all-on" baseline; feature-filtering tests are added
/// once --feature CLI support lands.
#[test]
fn snapshot_if_feature_all_enabled_tree() {
    snapshot(
        "if_feature",
        &["-f", "tree", "base.yang"],
        "base.tree.expected",
    );
}

#[test]
fn test_max_status_current() {
    let (stdout, stderr, code) = run(
        "max_status",
        &["-f", "tree", "--max-status", "current", "a.yang"],
    );
    assert_eq!(
        code, 0,
        "unexpected exit code\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("baz?"),
        "expected current leaf baz\n{stdout}"
    );
    assert!(
        !stdout.contains("foo?"),
        "deprecated leaf foo should be pruned\n{stdout}"
    );
    assert!(
        !stdout.contains("bar?"),
        "obsolete leaf bar should be pruned\n{stdout}"
    );
}

#[test]
fn test_max_status_deprecated() {
    let (stdout, stderr, code) = run(
        "max_status",
        &["-f", "tree", "--max-status", "deprecated", "a.yang"],
    );
    assert_eq!(
        code, 0,
        "unexpected exit code\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("foo?"),
        "expected deprecated leaf foo\n{stdout}"
    );
    assert!(
        stdout.contains("baz?"),
        "expected current leaf baz\n{stdout}"
    );
    assert!(
        !stdout.contains("bar?"),
        "obsolete leaf bar should be pruned\n{stdout}"
    );
}

#[test]
fn test_max_status_default() {
    let (stdout, stderr, code) = run("max_status", &["-f", "tree", "a.yang"]);
    assert_eq!(
        code, 0,
        "unexpected exit code\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("foo?"),
        "expected deprecated leaf foo\n{stdout}"
    );
    assert!(
        stdout.contains("bar?"),
        "expected obsolete leaf bar\n{stdout}"
    );
    assert!(
        stdout.contains("baz?"),
        "expected current leaf baz\n{stdout}"
    );
}

#[test]
fn test_feature_local_with_prefix() {
    let (stdout, stderr, code) = run(
        "ignore_unknown_features",
        &[
            "-f",
            "tree",
            "--feature",
            "unknown-feat:local-feat",
            "unknown_feat.yang",
        ],
    );
    assert_eq!(
        code, 0,
        "unexpected exit code\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert_contains_in_order(
        &stdout,
        &["module: unknown-feat", "with-local", "with-local-prefixed"],
    );
}

#[test]
fn test_feature_comma_separated() {
    let (stdout, stderr, code) = run(
        "if_feature",
        &[
            "-f",
            "tree",
            "--feature",
            "base:advanced,experimental",
            "base.yang",
        ],
    );
    assert_eq!(
        code, 0,
        "unexpected exit code\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert_contains_in_order(
        &stdout,
        &[
            "advanced-setting",
            "advanced-block",
            "experimental-leaf",
            "combined",
        ],
    );
    assert!(
        !stdout.contains("combo-leaf"),
        "combo-leaf should remain disabled without base:combo\n{stdout}"
    );
}

// ── new fixtures: deviation targeting a node inside a uses expansion ───────

/// groupmod.yang defines a grouping with container/ip/port/protocol.
/// restrict.yang deviates /gm:server/gm:address/gm:port (not-supported)
/// and replaces /gm:server/gm:address/gm:protocol default.
/// The port leaf must be absent; protocol must remain (default change is not
/// visible in tree output but must not cause a crash).
#[test]
fn snapshot_deviation_in_uses_tree() {
    snapshot(
        "deviation_in_uses",
        &["-f", "tree", "groupmod.yang", "restrict.yang"],
        "groupmod_restrict.tree.expected",
    );
}

#[test]
fn deviation_in_uses_port_removed() {
    let (stdout, _stderr, code) = run(
        "deviation_in_uses",
        &["-f", "tree", "groupmod.yang", "restrict.yang"],
    );
    assert_eq!(code, 0, "unexpected exit code\nstdout: {stdout}");
    let has_port = stdout
        .lines()
        .any(|l| l.contains("+--") && l.contains("port"));
    assert!(
        !has_port,
        "port should have been removed by not-supported deviation\n{stdout}"
    );
    assert!(
        stdout.contains("protocol"),
        "protocol should remain\n{stdout}"
    );
}

// ── new fixtures: combined refine + deviation on same node ─────────────────

/// rdmod.yang uses a grouping with refine (timeout default 30→60, retries
/// mandatory→optional with default 3).
/// override.yang then deviates /rd:settings/rd:timeout (default 60→120)
/// and /rd:settings/rd:mode (not-supported).
/// mode must be absent; timeout and retries must be optional.
#[test]
fn snapshot_refine_deviate_tree() {
    snapshot(
        "refine_deviate",
        &["-f", "tree", "rdmod.yang", "override.yang"],
        "rdmod_override.tree.expected",
    );
}

#[test]
fn refine_deviate_mode_removed_timeout_optional() {
    let (stdout, _stderr, code) = run(
        "refine_deviate",
        &["-f", "tree", "rdmod.yang", "override.yang"],
    );
    assert_eq!(code, 0, "unexpected exit code\nstdout: {stdout}");
    let has_mode = stdout
        .lines()
        .any(|l| l.contains("+--") && l.contains("mode"));
    assert!(
        !has_mode,
        "mode should have been removed by not-supported deviation\n{stdout}"
    );
    // timeout and retries made optional by refine; both should appear as ?
    assert!(
        stdout
            .lines()
            .any(|l| l.contains("timeout?") || l.contains("timeout?")),
        "timeout should be optional\n{stdout}"
    );
    assert!(
        stdout.lines().any(|l| l.contains("retries?")),
        "retries should be optional (refine mandatory false)\n{stdout}"
    );
}

// ── generate-bundle subcommand ───────────────────────────────────────────────

/// `generate-bundle` walks a tree and classifies each file by role: plain
/// modules → `modules`, deviation modules → `deviation_modules`, submodules →
/// their directory under `search_paths` (never listed as a primary target).
#[test]
fn generate_bundle_classifies_by_role() {
    let dir = yang_dir("genbundle");
    let out = Command::new(yangest_bin())
        .arg("generate-bundle")
        .arg(&dir)
        .output()
        .expect("failed to run yangest generate-bundle");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(0), "unexpected exit\nstderr: {}", String::from_utf8_lossy(&out.stderr));

    // prim.yang is primary; dev.yang (has a `deviation`) is a deviation module,
    // listed after the modules array.
    assert_contains_in_order(
        &stdout,
        &["modules = [", "prim.yang", "deviation_modules = [", "dev.yang"],
    );
    // The submodule's directory is carried into search_paths so `include` resolves.
    assert_contains_in_order(&stdout, &["search_paths = [", "nested"]);
    // The submodule file itself is never listed (not a primary, not by filename).
    assert!(
        !stdout.contains("sub.yang"),
        "submodule sub.yang must not be listed as an input\n{stdout}"
    );
    // dev.yang must not appear in the primary `modules` array (it precedes the
    // deviation_modules header).
    let modules_section = stdout.split("deviation_modules").next().unwrap_or("");
    assert!(
        !modules_section.contains("dev.yang"),
        "deviation module must not be listed as primary\n{stdout}"
    );
}

// ── revision resolution (RFC 7950 §5.1.1) ────────────────────────────────────

/// A revision-less `include` must resolve to the latest submodule revision. The
/// `revisions/` fixture has `sub@2024-01-01` (grouping g → old_leaf) and
/// `sub@2026-06-07` (grouping g → new_leaf); `parent` does `include sub; uses g;`.
#[test]
fn include_without_revision_uses_latest_submodule() {
    let dir = yang_dir("revisions");
    let out = Command::new(yangest_bin())
        .arg("-f")
        .arg("tree")
        .arg("-p")
        .arg(&dir)
        .arg(dir.join("parent.yang"))
        .output()
        .expect("failed to run yangest");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("new_leaf"), "expected the latest submodule revision\n{stdout}");
    assert!(!stdout.contains("old_leaf"), "must not use the old submodule revision\n{stdout}");
}

/// `generate-bundle` lists only the latest revision of a module that appears in
/// several revisions (`thing@2024-01-01` and `thing@2026-06-07`).
#[test]
fn generate_bundle_lists_latest_revision() {
    let dir = yang_dir("revisions");
    let out = Command::new(yangest_bin())
        .arg("generate-bundle")
        .arg(&dir)
        .output()
        .expect("failed to run yangest generate-bundle");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    // Consider only listed entries, not `# note:` comments (which mention the
    // superseded revision by name).
    let listed = |needle: &str| {
        stdout
            .lines()
            .any(|l| !l.trim_start().starts_with('#') && l.contains(needle))
    };
    assert!(listed("thing@2026-06-07.yang"), "latest revision should be listed\n{stdout}");
    assert!(!listed("thing@2024-01-01.yang"), "older revision must not be listed\n{stdout}");
}

// ── plugin CLI options + diagnostics + --werror ───────────────────────────────

/// A plugin declares a CLI flag (Part A) which, when set, raises a warning
/// (Part B). Without the flag the run is clean.
#[test]
fn plugin_cli_flag_and_diagnostics() {
    // Flag off: no warning, exit 0.
    let (_out, err, code) = run("werror", &["-f", "bundle-imports", "base.yang", "main.yang"]);
    assert_eq!(code, 0, "clean run\n{err}");
    assert!(!err.contains("not revision-pinned"), "no warning without the flag\n{err}");

    // Flag on: the plugin raises a warning on stderr but the run still succeeds.
    let (_out, err, code) = run(
        "werror",
        &["-f", "bundle-imports", "--bundle-imports-warn-unpinned", "base.yang", "main.yang"],
    );
    assert_eq!(code, 0, "warnings alone do not fail\n{err}");
    assert!(
        err.contains("bundle-imports: warning: [unpinned-import]"),
        "plugin warning is reported\n{err}"
    );
    assert!(err.contains("import of 'base' is not revision-pinned"), "{err}");
}

/// `--werror` promotes the plugin warning to a fatal error (non-zero exit).
#[test]
fn werror_promotes_plugin_warning() {
    let (_out, err, code) = run(
        "werror",
        &[
            "-f",
            "bundle-imports",
            "--bundle-imports-warn-unpinned",
            "--werror",
            "base.yang",
            "main.yang",
        ],
    );
    assert_eq!(code, 1, "--werror makes the warning fatal\n{err}");
    assert!(err.contains("not revision-pinned"), "the warning is still shown\n{err}");
}

/// `--werror` without any warnings does not fail a clean run.
#[test]
fn werror_clean_run_succeeds() {
    let (_out, _err, code) = run(
        "werror",
        &["-f", "bundle-imports", "--werror", "base.yang", "main.yang"],
    );
    assert_eq!(code, 0, "no warnings -> --werror is a no-op");
}
