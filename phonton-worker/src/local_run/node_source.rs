//! Conservative Node source inclusion from process-reported V8 coverage.
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

const MAX_COVERAGE_FILES: usize = 64;
const MAX_COVERAGE_BYTES: u64 = 16 * 1024 * 1024;

pub(super) fn changed_sources(changes: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    changes
        .iter()
        .filter(|path| is_source(path))
        .cloned()
        .collect()
}

pub(super) fn is_source(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "js" | "cjs" | "mjs" | "jsx" | "ts" | "cts" | "mts" | "tsx"
            )
        })
}

pub(super) fn observed_sources(
    coverage_dir: &Path,
    candidate_dir: &Path,
) -> Result<BTreeSet<PathBuf>, String> {
    let root = candidate_dir
        .canonicalize()
        .map_err(|error| format!("Candidate path unavailable: {error}"))?;
    let mut observed = BTreeSet::new();
    let mut files = 0;
    for entry in fs::read_dir(coverage_dir)
        .map_err(|error| format!("Coverage folder unavailable: {error}"))?
    {
        let entry = entry.map_err(|error| format!("Coverage entry unavailable: {error}"))?;
        files += 1;
        if files > MAX_COVERAGE_FILES {
            return Err("Coverage file count exceeds inspection bound".into());
        }
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| format!("Coverage metadata unavailable: {error}"))?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_COVERAGE_BYTES {
            return Err(
                "Coverage output is linked, not a file, or exceeds inspection bound".into(),
            );
        }
        let bytes =
            fs::read(entry.path()).map_err(|error| format!("Coverage file unreadable: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Coverage JSON invalid: {error}"))?;
        let scripts = value
            .get("result")
            .and_then(serde_json::Value::as_array)
            .ok_or("Coverage result missing")?;
        for script in scripts {
            let Some(url) = script.get("url").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(path) = reqwest::Url::parse(url)
                .ok()
                .and_then(|url| url.to_file_path().ok())
            else {
                continue;
            };
            if let Ok(path) = path.canonicalize() {
                if path.starts_with(&root) {
                    observed.insert(path);
                }
            }
        }
    }
    if files == 0 {
        return Err("Node did not write V8 coverage output".into());
    }
    Ok(observed)
}
