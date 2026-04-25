use std::path::Path;

use phonton_index::extract_symbols;

#[test]
fn extracts_symbols_from_phonton_types() {
    // CARGO_MANIFEST_DIR is the phonton-index package root at compile time.
    let source_path = format!(
        "{}/../phonton-types/src/lib.rs",
        env!("CARGO_MANIFEST_DIR")
    );

    let source = std::fs::read_to_string(&source_path)
        .unwrap_or_else(|e| panic!("could not read {source_path}: {e}"));

    let symbols = extract_symbols(&source, Path::new("phonton-types/src/lib.rs"));

    println!("\n=== extracted symbols ({}) ===", symbols.len());
    for s in &symbols {
        println!(
            "  [{:?}] {} — {} tokens, doc: {}",
            s.file_path,
            s.symbol_name,
            s.token_count,
            s.docstring.as_deref().unwrap_or("none"),
        );
    }
    println!("===\n");

    assert!(
        symbols.len() > 5,
        "expected >5 symbols, got {}",
        symbols.len()
    );

    let names: Vec<&str> = symbols.iter().map(|s| s.symbol_name.as_str()).collect();

    for expected in &["TaskId", "ModelTier", "DiffHunk"] {
        assert!(
            names.contains(expected),
            "symbol {expected:?} not found; got: {names:?}"
        );
    }

    for s in &symbols {
        assert!(
            !s.symbol_name.is_empty(),
            "symbol with empty name found: {s:?}"
        );
    }
}
