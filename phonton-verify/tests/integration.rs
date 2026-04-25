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

use phonton_types::{DiffHunk, DiffLine, VerifyLayer, VerifyResult};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a `DiffHunk` targeting `path` with the given added lines.
fn hunk_added(path: &str, lines: Vec<&str>) -> DiffHunk {
    DiffHunk {
        file_path: PathBuf::from(path),
        old_start: 1,
        old_count: 0,
        new_start: 1,
        new_count: lines.len() as u32,
        lines: lines.into_iter().map(|l| DiffLine::Added(l.into())).collect(),
    }
}

/// Scaffold a minimal Cargo project inside `dir` and return its root path.
/// The crate name is always `test_crate` so the verify pipeline can infer
/// it from the package name in Cargo.toml (not from path-based heuristics
/// which look for `phonton-*` prefixes).
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

// ---------------------------------------------------------------------------
// Test 1: broken syntax → Fail at Layer 1 (Syntax)
// ---------------------------------------------------------------------------

/// Tree-sitter should catch obviously broken Rust syntax before cargo is ever
/// invoked. This is the cheapest possible fail — no subprocess spawned.
#[tokio::test]
async fn broken_syntax_fails_at_layer_1() {
    let hunk = hunk_added(
        "phonton-types/src/broken.rs",
        vec!["fn broken( -> {"],
    );

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
    let root = scaffold_cargo_project(
        &dir,
        "pub fn foo() -> NonExistentType { todo!() }\n",
    );

    // The hunk must have a file_path that the verify pipeline can map to a
    // package. Since our crate is named `test_crate` (no `phonton-` prefix),
    // `touched_packages` won't find it via path heuristics. Instead we call
    // `verify_crate_check` directly with the known package name.
    let hunk = hunk_added(
        "src/lib.rs",
        vec!["pub fn foo() -> NonExistentType { todo!() }"],
    );

    // Verify syntax passes first (the code is syntactically valid).
    let syntax = phonton_verify::verify_syntax(&[hunk]);
    assert!(syntax.is_none(), "syntax should pass for valid parse tree");

    // Now run crate check directly.
    let packages = vec!["test_crate".to_string()];
    let result = phonton_verify::verify_crate_check(&packages, &root)
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
// Test 3: valid code → Pass all layers
// ---------------------------------------------------------------------------

/// A well-formed Cargo project should sail through syntax, crate check,
/// workspace check, and tests (the default test suite is empty = passes).
#[tokio::test]
async fn valid_code_passes_all_layers() {
    let dir = TempDir::new().expect("create temp dir");
    let root = scaffold_cargo_project(
        &dir,
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    );

    let hunk = hunk_added(
        "src/lib.rs",
        vec!["pub fn add(a: i32, b: i32) -> i32 { a + b }"],
    );

    // Syntax check (Layer 1).
    let syntax = phonton_verify::verify_syntax(&[hunk]);
    assert!(syntax.is_none(), "syntax should pass");

    // Crate check (Layer 2).
    let packages = vec!["test_crate".to_string()];
    let l2 = phonton_verify::verify_crate_check(&packages, &root)
        .await
        .expect("crate check should not error");
    assert!(l2.is_none(), "crate check should pass, got: {l2:?}");

    // Workspace check (Layer 3).
    let l3 = phonton_verify::verify_workspace_check(&root)
        .await
        .expect("workspace check should not error");
    assert!(l3.is_none(), "workspace check should pass, got: {l3:?}");

    // Test (Layer 4) — empty test suite passes.
    let l4 = phonton_verify::verify_test(&packages, &root)
        .await
        .expect("test should not error");
    assert!(l4.is_none(), "test should pass, got: {l4:?}");
}

// ---------------------------------------------------------------------------
// Test 4: syntax-only fast-path is sub-millisecond
// ---------------------------------------------------------------------------

/// Syntax verification should be very fast — it's just tree-sitter parsing
/// with no subprocess. This test asserts it completes under 50ms to catch
/// accidental subprocess invocations.
#[tokio::test]
async fn syntax_check_is_fast() {
    let hunk = hunk_added(
        "phonton-types/src/fast.rs",
        vec!["fn ok() -> u32 { 42 }"],
    );

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
    let l2 = phonton_verify::verify_crate_check(&packages, &root)
        .await
        .expect("crate check should not error");
    assert!(l2.is_none(), "crate check should pass, got: {l2:?}");

    // Test should fail.
    let result = phonton_verify::verify_test(&packages, &root)
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
