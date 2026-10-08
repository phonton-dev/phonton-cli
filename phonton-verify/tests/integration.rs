//! Integration tests for the layered verification pipeline.
//!
//! These tests spin up real `cargo check` / `cargo test` processes and require
//! a working Rust toolchain on PATH. Gate behind the `integration-tests`
//! feature so CI can skip them in constrained environments:
//!
//! ```bash
//! cargo test -p phonton-verify --features integration-tests
//! ```

#![cfg(feature = "integration-tests")]

use std::path::PathBuf;
use std::time::Instant;

use phonton_types::verification::VerificationExecution;
use phonton_types::{DiffHunk, DiffLine, VerifyLayer, VerifyResult};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a `DiffHunk` targeting `path` with the given added lines.
fn hunk_added(path: &str, lines: Vec<&str>) -> DiffHunk {
    DiffHunk {
        file_path: PathBuf::from(path),
        old_start: 0,
        old_count: 0,
        new_start: 1,
        new_count: lines.len() as u32,
        lines: lines
            .into_iter()
            .map(|l| DiffLine::Added(l.into()))
            .collect(),
    }
}

/// Scaffold a minimal Cargo project inside `dir` and return its root path.
/// The crate name is always `test_crate` in Cargo.toml.
fn scaffold_cargo_project(dir: &TempDir, lib_content: &str) -> PathBuf {
    let root = dir.path().to_path_buf();

    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "test_crate"
version = "0.1.0"
edition = "2021"
"#,
    )
    .expect("write Cargo.toml");

    std::fs::create_dir_all(root.join("src")).expect("create src dir");
    std::fs::write(root.join("src").join("lib.rs"), lib_content).expect("write lib.rs");
    root
}

#[tokio::test]
async fn root_library_candidate_runs_tests_and_reports_test_layer() {
    let dir = TempDir::new().expect("create temp dir");
    let root = scaffold_cargo_project(
        &dir,
        "pub fn add_one(n: i32) -> i32 { n }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn adds_one() { assert_eq!(super::add_one(1), 2); }\n}\n",
    );

    let candidate = |replacement: &str| DiffHunk {
        file_path: PathBuf::from("src/lib.rs"),
        old_start: 1,
        old_count: 1,
        new_start: 1,
        new_count: 1,
        lines: vec![
            DiffLine::Removed("pub fn add_one(n: i32) -> i32 { n }".into()),
            DiffLine::Added(replacement.into()),
        ],
    };

    let passing = phonton_verify::verify_diff_with_execution(
        &[candidate("pub fn add_one(n: i32) -> i32 { n + 1 }")],
        &root,
        None,
        VerificationExecution::HostApproved,
    )
    .await
    .expect("verify passing candidate");
    assert_eq!(
        passing,
        VerifyResult::Pass {
            layer: VerifyLayer::Test
        }
    );

    let failing = phonton_verify::verify_diff_with_execution(
        &[candidate("pub fn add_one(n: i32) -> i32 { n + 0 }")],
        &root,
        None,
        VerificationExecution::HostApproved,
    )
    .await
    .expect("verify failing candidate");
    assert!(matches!(
        failing,
        VerifyResult::Fail {
            layer: VerifyLayer::Test,
            ..
        }
    ));
    assert!(std::fs::read_to_string(root.join("src/lib.rs"))
        .expect("read original")
        .contains("{ n }"));
}

#[tokio::test]
async fn single_quoted_package_name_still_runs_candidate_tests() {
    let dir = TempDir::new().expect("create temp dir");
    let root = scaffold_cargo_project(
        &dir,
        "pub fn add_one(n: i32) -> i32 { n }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn adds_one() { assert_eq!(super::add_one(1), 2); }\n}\n",
    );
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = 'test_crate'\nversion = '0.1.0'\nedition = '2021'\n",
    )
    .expect("write valid single-quoted manifest");
    let candidate = DiffHunk {
        file_path: PathBuf::from("src/lib.rs"),
        old_start: 1,
        old_count: 1,
        new_start: 1,
        new_count: 1,
        lines: vec![
            DiffLine::Removed("pub fn add_one(n: i32) -> i32 { n }".into()),
            DiffLine::Added("pub fn add_one(n: i32) -> i32 { n + 1 }".into()),
        ],
    };

    assert_eq!(
        phonton_verify::verify_diff_with_execution(
            &[candidate],
            &root,
            None,
            VerificationExecution::HostApproved,
        )
        .await
        .expect("verify candidate"),
        VerifyResult::Pass {
            layer: VerifyLayer::Test
        }
    );
}

#[tokio::test]
async fn empty_package_does_not_hide_later_failing_package_tests() {
    let dir = TempDir::new().expect("create temp dir");
    let root = dir.path();
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"empty\", \"failing\"]\nresolver = \"2\"\n",
    )
    .expect("write workspace manifest");
    for (name, source) in [
        ("empty", "pub fn value() -> i32 { 1 }\n"),
        (
            "failing",
            "pub fn value() -> i32 { 1 }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn needs_three() { assert_eq!(super::value(), 3); }\n}\n",
        ),
    ] {
        std::fs::create_dir_all(root.join(name).join("src")).expect("create package");
        std::fs::write(
            root.join(name).join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .expect("write package manifest");
        std::fs::write(root.join(name).join("src/lib.rs"), source).expect("write source");
    }
    let candidate = |name: &str| DiffHunk {
        file_path: PathBuf::from(name).join("src/lib.rs"),
        old_start: 1,
        old_count: 1,
        new_start: 1,
        new_count: 1,
        lines: vec![
            DiffLine::Removed("pub fn value() -> i32 { 1 }".into()),
            DiffLine::Added("pub fn value() -> i32 { 2 }".into()),
        ],
    };

    let result = phonton_verify::verify_diff_with_execution(
        &[candidate("empty"), candidate("failing")],
        root,
        None,
        VerificationExecution::HostApproved,
    )
    .await
    .expect("verify both packages");
    assert!(matches!(
        result,
        VerifyResult::Fail {
            layer: VerifyLayer::Test,
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// Test 1: broken syntax → Fail at Layer 1 (Syntax)
// ---------------------------------------------------------------------------

/// Tree-sitter should catch obviously broken Rust syntax before cargo is ever
/// invoked. This is the cheapest possible fail — no subprocess spawned.
#[tokio::test]
async fn broken_syntax_fails_at_layer_1() {
    let hunk = hunk_added("phonton-types/src/broken.rs", vec!["fn broken( -> {"]);

    let dir = TempDir::new().expect("create temp dir");
    let result = phonton_verify::verify_diff(&[hunk], dir.path())
        .await
        .expect("verify_diff should not error");

    match result {
        VerifyResult::Fail {
            layer: VerifyLayer::Syntax,
            ref errors,
            ..
        } => {
            assert!(!errors.is_empty(), "should have at least one syntax error");
            // Confirm the error references the file path
            let joined = errors.join("\n");
            assert!(
                joined.contains("broken.rs"),
                "error should mention the broken file, got: {joined}"
            );
        }
        other => panic!("expected Fail at Syntax layer, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test 2: valid syntax, broken types → Fail at Layer 2 (CrateCheck)
// ---------------------------------------------------------------------------

/// Code that parses fine syntactically but fails `cargo check` should be
/// caught at Layer 2. We create a real Cargo project with a type error.
#[tokio::test]
async fn broken_types_fails_at_layer_2() {
    let dir = TempDir::new().expect("create temp dir");
    let root = scaffold_cargo_project(&dir, "pub fn foo() -> NonExistentType { todo!() }\n");

    // Exercise the crate-check layer directly with a known package name so
    // this test isolates compiler failure from the rest of the pipeline.
    let hunk = hunk_added(
        "src/lib.rs",
        vec!["pub fn foo() -> NonExistentType { todo!() }"],
    );

    // Verify syntax passes first (the code is syntactically valid).
    let syntax = phonton_verify::verify_syntax(&[hunk]);
    assert!(syntax.is_none(), "syntax should pass for valid parse tree");

    // Now run crate check directly.
    let packages = vec!["test_crate".to_string()];
    let result = phonton_verify::verify_crate_check_with_execution(
        &packages,
        &root,
        VerificationExecution::HostApproved,
    )
    .await
    .expect("verify_crate_check should not error");

    match result {
        Some(VerifyResult::Fail {
            layer: VerifyLayer::CrateCheck,
            ref errors,
            ..
        }) => {
            assert!(!errors.is_empty(), "should have at least one type error");
            let joined = errors.join("\n").to_lowercase();
            assert!(
                joined.contains("cannot find type")
                    || joined.contains("not found")
                    || joined.contains("nonexistenttype"),
                "error should mention the missing type, got: {joined}"
            );
        }
        Some(other) => panic!("expected Fail at CrateCheck layer, got: {other:?}"),
        None => panic!("expected crate check to fail, but it passed"),
    }
}

// ---------------------------------------------------------------------------
// Test 3: valid code with no tests has no test evidence
// ---------------------------------------------------------------------------

/// A well-formed Cargo project passes checks, but an empty default test suite
/// must not count as completed behavioral verification.
#[tokio::test]
async fn valid_code_without_tests_does_not_claim_test_pass() {
    let dir = TempDir::new().expect("create temp dir");
    let root = scaffold_cargo_project(&dir, "pub fn add(a: i32, b: i32) -> i32 { a + b }\n");

    let hunk = hunk_added(
        "src/lib.rs",
        vec!["pub fn add(a: i32, b: i32) -> i32 { a + b }"],
    );

    // Syntax check (Layer 1).
    let syntax = phonton_verify::verify_syntax(&[hunk]);
    assert!(syntax.is_none(), "syntax should pass");

    // Crate check (Layer 2).
    let packages = vec!["test_crate".to_string()];
    let l2 = phonton_verify::verify_crate_check_with_execution(
        &packages,
        &root,
        VerificationExecution::HostApproved,
    )
    .await
    .expect("crate check should not error");
    assert!(l2.is_none(), "crate check should pass, got: {l2:?}");

    // Workspace check (Layer 3).
    let l3 = phonton_verify::verify_workspace_check_with_execution(
        &root,
        VerificationExecution::HostApproved,
    )
    .await
    .expect("workspace check should not error");
    assert!(l3.is_none(), "workspace check should pass, got: {l3:?}");

    // Test (Layer 4) — an empty test suite cannot certify behavior.
    let l4 = phonton_verify::verify_test_with_execution(
        &packages,
        &root,
        VerificationExecution::HostApproved,
    )
    .await
    .expect("test should not error");
    assert!(matches!(l4, Some(VerifyResult::NotRun { .. })));
}

// ---------------------------------------------------------------------------
// Test 4: syntax-only fast-path is sub-millisecond
// ---------------------------------------------------------------------------

/// Syntax verification should be very fast — it's just tree-sitter parsing
/// with no subprocess. This test asserts it completes under 50ms to catch
/// accidental subprocess invocations.
#[tokio::test]
async fn syntax_check_is_fast() {
    let hunk = hunk_added("phonton-types/src/fast.rs", vec!["fn ok() -> u32 { 42 }"]);

    let start = Instant::now();
    let result = phonton_verify::verify_syntax(&[hunk]);
    let elapsed = start.elapsed();

    assert!(result.is_none(), "valid code should pass syntax check");
    assert!(
        elapsed.as_millis() < 50,
        "syntax check took {elapsed:?} — should be under 50ms"
    );
}

// ---------------------------------------------------------------------------
// Test 5: non-Rust files skip syntax check
// ---------------------------------------------------------------------------

/// Hunks targeting non-Rust files should pass the syntax layer unconditionally
/// because tree-sitter-rust only handles `.rs` files.
#[tokio::test]
async fn non_rust_files_skip_syntax() {
    let hunk = hunk_added(
        "phonton-types/src/config.toml",
        vec!["this is = definitely not valid rust"],
    );

    let result = phonton_verify::verify_syntax(&[hunk]);
    assert!(
        result.is_none(),
        "non-Rust file should skip syntax check, got: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 6: failing tests → Fail at Layer 4
// ---------------------------------------------------------------------------

/// A project with a failing test should pass Layers 1-3 but fail at Layer 4.
#[tokio::test]
async fn failing_test_fails_at_layer_4() {
    let dir = TempDir::new().expect("create temp dir");
    let root = scaffold_cargo_project(
        &dir,
        r#"pub fn add(a: i32, b: i32) -> i32 { a + b }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_fails() {
        assert_eq!(add(1, 2), 999, "deliberately broken");
    }
}
"#,
    );

    // Crate check should pass (the code compiles).
    let packages = vec!["test_crate".to_string()];
    let l2 = phonton_verify::verify_crate_check_with_execution(
        &packages,
        &root,
        VerificationExecution::HostApproved,
    )
    .await
    .expect("crate check should not error");
    assert!(l2.is_none(), "crate check should pass, got: {l2:?}");

    // Test should fail.
    let result = phonton_verify::verify_test_with_execution(
        &packages,
        &root,
        VerificationExecution::HostApproved,
    )
    .await
    .expect("verify_test should not error");

    match result {
        Some(VerifyResult::Fail {
            layer: VerifyLayer::Test,
            ref errors,
            ..
        }) => {
            assert!(!errors.is_empty(), "should have test failure output");
            let joined = errors.join("\n");
            assert!(
                joined.contains("deliberately broken") || joined.contains("FAILED"),
                "error should contain test failure details, got: {joined}"
            );
        }
        Some(other) => panic!("expected Fail at Test layer, got: {other:?}"),
        None => panic!("expected test to fail, but it passed"),
    }
}
