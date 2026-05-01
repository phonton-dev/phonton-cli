//! Semantic code index: tree-sitter parsing + heuristic fallbacks +
//! HNSW-based vector retrieval over extracted symbols.

use anyhow::{Context, Result};
use phonton_types::{CodeSlice, SliceOrigin};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Symbol kinds extracted from source files.
const RUST_KINDS: &[&str] = &[
    "function_item",
    "impl_item",
    "struct_item",
    "enum_item",
    "trait_item",
    "type_alias",
];
const PYTHON_KINDS: &[&str] = &["function_definition", "class_definition"];
const TS_KINDS: &[&str] = &[
    "function_declaration",
    "class_description",
    "interface_declaration",
    "method_definition",
];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse `source` and return a [`CodeSlice`] for every symbol found.
///
/// Supported languages (Semantic): Rust, Python, TypeScript.
/// Others: Fallback (regex heuristic).
pub fn extract_symbols(source: &str, file_path: &Path) -> Vec<CodeSlice> {
    let ext = file_path.extension().and_then(|s| s.to_str()).unwrap_or("");

    match ext {
        "rs" => extract_semantic(source, file_path, tree_sitter_rust::language(), RUST_KINDS),
        "py" => extract_semantic(
            source,
            file_path,
            tree_sitter_python::language(),
            PYTHON_KINDS,
        ),
        "ts" | "js" | "tsx" | "jsx" => extract_semantic(
            source,
            file_path,
            tree_sitter_typescript::language_typescript(),
            TS_KINDS,
        ),
        _ => extract_fallback(source, file_path),
    }
}

fn extract_semantic(
    source: &str,
    file_path: &Path,
    lang: tree_sitter::Language,
    kinds: &[&str],
) -> Vec<CodeSlice> {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&lang).is_err() {
        return extract_fallback(source, file_path);
    }

    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return extract_fallback(source, file_path),
    };

    let mut symbols = Vec::new();
    collect_symbols(
        tree.root_node(),
        source.as_bytes(),
        file_path,
        kinds,
        &mut symbols,
    );
    symbols
}

fn extract_fallback(source: &str, file_path: &Path) -> Vec<CodeSlice> {
    let mut symbols = Vec::new();
    // Risk 3: line-based grep for common patterns.
    let re = Regex::new(r"(?m)^(?:pub\s+)?(?:fn|struct|enum|trait|class|def|function|interface)\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap();

    for cap in re.captures_iter(source) {
        let name = cap[1].to_string();
        let full_match = cap.get(0).unwrap();
        let start = full_match.start();

        // Grab a few lines of context.
        let end = (start + 200).min(source.len());
        let signature = source[start..end].to_string();

        symbols.push(CodeSlice {
            file_path: file_path.to_path_buf(),
            symbol_name: name,
            token_count: estimate_tokens(&signature),
            signature,
            docstring: None,
            callsites: Vec::new(),
            origin: SliceOrigin::Fallback,
        });
    }
    symbols
}

// ---------------------------------------------------------------------------
// AST traversal (Semantic)
// ---------------------------------------------------------------------------

fn collect_symbols(
    node: tree_sitter::Node,
    source: &[u8],
    file_path: &Path,
    kinds: &[&str],
    out: &mut Vec<CodeSlice>,
) {
    for i in 0..node.child_count() {
        let child = match node.child(i) {
            Some(c) => c,
            None => continue,
        };

        if kinds.contains(&child.kind()) {
            if let Some(symbol_name) = extract_name(child, source) {
                let signature = extract_signature(child, source);
                let token_count = estimate_tokens(&signature);
                out.push(CodeSlice {
                    file_path: file_path.to_path_buf(),
                    symbol_name,
                    signature,
                    docstring: None,
                    callsites: Vec::new(),
                    token_count,
                    origin: SliceOrigin::Semantic,
                });
            }
        }
        collect_symbols(child, source, file_path, kinds, out);
    }
}

fn extract_name(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok())
        .map(String::from)
}

fn extract_signature(node: tree_sitter::Node, source: &[u8]) -> String {
    let text = node.utf8_text(source).unwrap_or("").trim();
    if text.len() <= 400 {
        return text.to_string();
    }
    text[..400].to_string()
}

fn estimate_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    if chars == 0 {
        0
    } else {
        chars.div_ceil(4).max(1)
    }
}

// ---------------------------------------------------------------------------
// Semantic retrieval: embeddings + HNSW
// ---------------------------------------------------------------------------

/// Dimension of `all-MiniLM-L6-v2` sentence embeddings.
#[cfg(feature = "semantic")]
pub const EMBED_DIM: usize = 384;

/// Wrapper around a loaded fastembed `TextEmbedding` model.
#[cfg(feature = "semantic")]
pub struct Embedder {
    model: fastembed::TextEmbedding,
}

#[cfg(feature = "semantic")]
impl Embedder {
    /// Load `all-MiniLM-L6-v2`. On first call this downloads and caches
    /// the ONNX weights under the platform's fastembed cache dir.
    pub fn new() -> Result<Self> {
        let model = fastembed::TextEmbedding::try_new(fastembed::InitOptions::new(
            fastembed::EmbeddingModel::AllMiniLML6V2,
        ))
        .context("loading all-MiniLM-L6-v2")?;
        Ok(Self { model })
    }

    /// Embed a batch of texts. Returns one 384-dim vector per input.
    pub fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let docs: Vec<&str> = texts.to_vec();
        self.model
            .embed(docs, None)
            .map_err(|e| anyhow::anyhow!("fastembed: {e}"))
    }
}

/// HNSW index over [`CodeSlice`]s, keyed by insertion order.
#[cfg(feature = "semantic")]
pub struct SemanticIndex {
    index: usearch::Index,
    slots: Vec<CodeSlice>,
    embeddings: Vec<Vec<f32>>,
    dim: usize,
    pub file_hashes: std::collections::HashMap<PathBuf, u64>,
}

#[cfg(feature = "semantic")]
impl SemanticIndex {
    /// Build a new empty index with cosine distance over `dim`-dim vectors.
    pub fn new(dim: usize) -> Self {
        let options = usearch::IndexOptions {
            dimensions: dim,
            metric: usearch::MetricKind::Cos,
            quantization: usearch::ScalarKind::F32,
            ..Default::default()
        };
        let index = usearch::new_index(&options).expect("usearch::new_index");
        index.reserve(64).ok();
        Self {
            index,
            slots: Vec::new(),
            embeddings: Vec::new(),
            dim,
            file_hashes: std::collections::HashMap::new(),
        }
    }

    /// Add one slice + its precomputed embedding.
    pub fn add(&mut self, slice: CodeSlice, embedding: Vec<f32>) -> Result<()> {
        if embedding.len() != self.dim {
            return Err(anyhow::anyhow!(
                "embedding dim {} != index dim {}",
                embedding.len(),
                self.dim
            ));
        }
        let key = self.slots.len() as u64;
        if self.index.capacity() <= self.slots.len() {
            self.index
                .reserve(self.slots.len().saturating_add(64))
                .map_err(|e| anyhow::anyhow!("usearch reserve: {e}"))?;
        }
        self.index
            .add(key, &embedding)
            .map_err(|e| anyhow::anyhow!("usearch add: {e}"))?;
        self.slots.push(slice);
        self.embeddings.push(embedding);
        Ok(())
    }

    /// Remove all indexed slices associated with a specific file path.
    pub fn remove_by_file(&mut self, path: &Path) {
        let old_len = self.slots.len();
        let mut i = 0;
        while i < self.slots.len() {
            if self.slots[i].file_path == path {
                self.slots.remove(i);
                self.embeddings.remove(i);
            } else {
                i += 1;
            }
        }

        if self.slots.len() < old_len {
            let options = usearch::IndexOptions {
                dimensions: self.dim,
                metric: usearch::MetricKind::Cos,
                quantization: usearch::ScalarKind::F32,
                ..Default::default()
            };
            self.index = usearch::new_index(&options).expect("usearch::new_index");
            self.index.reserve(self.slots.len().saturating_add(64)).ok();
            for (idx, emb) in self.embeddings.iter().enumerate() {
                self.index.add(idx as u64, emb).expect("usearch add");
            }
        }
        self.file_hashes.remove(path);
    }

    /// Cosine-nearest `k` slices to `query_embedding`.
    pub fn search(&self, query_embedding: &[f32], k: usize) -> Vec<CodeSlice> {
        let matches = match self.index.search(query_embedding, k) {
            Ok(m) => m,
            Err(_) => return Vec::new(),
        };
        matches
            .keys
            .iter()
            .filter_map(|&key| self.slots.get(key as usize).cloned())
            .collect()
    }

    /// Current number of indexed slices.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// True if no slices are indexed.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Nexus: cross-repo indexing
// ---------------------------------------------------------------------------

/// Filename Phonton looks for when discovering a Nexus configuration.
pub const NEXUS_CONFIG_FILENAME: &str = "nexus.json";

/// One sibling repo declared in a [`NexusConfig`].
///
/// `path` is resolved relative to the directory containing the
/// `nexus.json` file. Both relative (`"../phonton-types"`) and absolute
/// paths are accepted; absolute paths are passed through unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NexusRepo {
    /// Human-readable label. Used for log lines and the error context
    /// when a repo can't be opened. Need not match the directory name.
    pub name: String,
    /// Path to the repo, relative to the `nexus.json` directory.
    pub path: PathBuf,
}

/// Top-level Nexus configuration.
///
/// A `nexus.json` declares a set of *sibling repos* whose source the
/// local index should pull into the same `SemanticIndex` as the current
/// `cwd`. This is how the ADE escapes the single-cwd trap: working in
/// `phonton-cli` can pull `phonton-types` context even when the two
/// crates live in separate git trees.
///
/// Unknown JSON fields are accepted and ignored so configs forward-
/// compatibly survive new keys added in later versions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NexusConfig {
    /// Schema version. Currently `1`.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Sibling repos to fold into the index.
    #[serde(default)]
    pub repos: Vec<NexusRepo>,
    /// Directory containing the `nexus.json`. Filled in by the loader;
    /// the JSON does not carry it.
    #[serde(skip)]
    pub config_dir: PathBuf,
}

fn default_version() -> u32 {
    1
}

impl NexusConfig {
    /// Resolve a [`NexusRepo::path`] against the config directory.
    /// Absolute paths are returned unchanged.
    pub fn resolve(&self, repo: &NexusRepo) -> PathBuf {
        if repo.path.is_absolute() {
            repo.path.clone()
        } else {
            self.config_dir.join(&repo.path)
        }
    }

    /// All resolved repo paths plus their human-readable names. Order
    /// matches the JSON; index callers walk them in declared order.
    pub fn resolved_repos(&self) -> Vec<(String, PathBuf)> {
        self.repos
            .iter()
            .map(|r| (r.name.clone(), self.resolve(r)))
            .collect()
    }
}

/// Load a `nexus.json` from an explicit path.
///
/// Returns `Err` if the file is missing, unreadable, or malformed.
/// `config_dir` is set to the parent directory of `path` so subsequent
/// resolution is anchored correctly.
pub fn load_nexus_config(path: &Path) -> Result<NexusConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading nexus config at {}", path.display()))?;
    let mut cfg: NexusConfig = serde_json::from_str(&raw)
        .with_context(|| format!("parsing nexus config at {}", path.display()))?;
    cfg.config_dir = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(cfg)
}

/// Walk upward from `start` looking for a `nexus.json`.
///
/// Returns `Ok(None)` if no config exists between `start` and the
/// filesystem root — the crate degrades gracefully to single-cwd
/// indexing in that case. Returns `Err` only when a `nexus.json` is
/// found but malformed.
pub fn discover_nexus_config(start: &Path) -> Result<Option<NexusConfig>> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(dir) = cur {
        let candidate = dir.join(NEXUS_CONFIG_FILENAME);
        if candidate.is_file() {
            return Ok(Some(load_nexus_config(&candidate)?));
        }
        cur = dir.parent();
    }
    Ok(None)
}

/// Build a single [`SemanticIndex`] spanning the local `root` plus
/// every sibling repo declared in `config`.
///
/// File paths are stored absolute so a downstream consumer can tell
/// which repo a slice came from by inspecting the prefix. A repo that
/// fails to open is logged and skipped — one broken sibling can't ground
/// the whole index. The local `root` is always indexed first, so its
/// slices land at the front of the slot list (slight stability win for
/// near-tie searches).
#[cfg(feature = "semantic")]
pub async fn index_workspace_with_nexus(
    root: &Path,
    config: &NexusConfig,
) -> Result<SemanticIndex> {
    let embedder = Embedder::new()?;
    index_workspace_with_nexus_using_embedder(root, config, &embedder).await
}

/// Build a Nexus-aware index using a caller-owned embedder.
///
/// This avoids loading the ONNX model twice when the same caller will also
/// use the embedder for query-time retrieval.
#[cfg(feature = "semantic")]
pub async fn index_workspace_with_nexus_using_embedder(
    root: &Path,
    config: &NexusConfig,
    embedder: &Embedder,
) -> Result<SemanticIndex> {
    let mut index = SemanticIndex::new(EMBED_DIM);
    let mut all_slices: Vec<CodeSlice> = Vec::new();

    // Local root first, then each declared sibling.
    let mut roots: Vec<(String, PathBuf)> = vec![("<local>".into(), root.to_path_buf())];
    roots.extend(config.resolved_repos());

    for (label, repo_root) in &roots {
        if !repo_root.exists() {
            // Missing sibling repo is a config error but not a fatal one
            // — surface and keep going.
            eprintln!(
                "phonton-index: nexus repo {label:?} at {} does not exist; skipping",
                repo_root.display()
            );
            continue;
        }
        let mut files: Vec<PathBuf> = Vec::new();
        if let Err(e) = collect_source_files(repo_root, &mut files) {
            eprintln!(
                "phonton-index: failed to walk nexus repo {label:?} ({}): {e}",
                repo_root.display()
            );
            continue;
        }
        for file in files {
            let Ok(source) = std::fs::read_to_string(&file) else {
                continue;
            };
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&source, &mut hasher);
            let hash = std::hash::Hasher::finish(&hasher);
            // Canonicalise so repos referenced via different relative
            // paths still hash to the same key.
            let key = std::fs::canonicalize(&file).unwrap_or(file.clone());
            index.file_hashes.insert(key, hash);
            for slice in extract_symbols(&source, &file) {
                all_slices.push(slice);
            }
        }
    }

    for chunk in all_slices.chunks(32) {
        let texts: Vec<&str> = chunk.iter().map(|s| s.signature.as_str()).collect();
        let vecs = embedder.embed(&texts)?;
        for (slice, vec) in chunk.iter().cloned().zip(vecs.into_iter()) {
            index.add(slice, vec)?;
        }
    }
    Ok(index)
}

/// Walk `root`, extract symbols from all Rust/Python/TypeScript sources,
/// embed their signatures in batches of 32, and return an HNSW index.
#[cfg(feature = "semantic")]
pub async fn index_workspace(root: &Path) -> Result<SemanticIndex> {
    let embedder = Embedder::new()?;
    index_workspace_using_embedder(root, &embedder).await
}

/// Build a single-workspace index using a caller-owned embedder.
#[cfg(feature = "semantic")]
pub async fn index_workspace_using_embedder(
    root: &Path,
    embedder: &Embedder,
) -> Result<SemanticIndex> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_source_files(root, &mut files)?;

    let mut all_slices: Vec<CodeSlice> = Vec::new();
    let mut index = SemanticIndex::new(EMBED_DIM);
    for file in files {
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&source, &mut hasher);
        let hash = std::hash::Hasher::finish(&hasher);
        index.file_hashes.insert(file.clone(), hash);

        for slice in extract_symbols(&source, &file) {
            all_slices.push(slice);
        }
    }

    for chunk in all_slices.chunks(32) {
        let texts: Vec<&str> = chunk.iter().map(|s| s.signature.as_str()).collect();
        let vecs = embedder.embed(&texts)?;
        for (slice, vec) in chunk.iter().cloned().zip(vecs.into_iter()) {
            index.add(slice, vec)?;
        }
    }
    Ok(index)
}

/// Embed `goal` and return the top-`k` most relevant indexed slices.
#[cfg(feature = "semantic")]
pub async fn query_relevant_slices(
    index: &SemanticIndex,
    embedder: &Embedder,
    goal: &str,
    k: usize,
) -> Vec<CodeSlice> {
    if index.is_empty() {
        return Vec::new();
    }
    let vecs = match embedder.embed(&[goal]) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(query) = vecs.into_iter().next() else {
        return Vec::new();
    };
    index.search(&query, k)
}

#[cfg(any(feature = "semantic", test))]
fn collect_source_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let entries = std::fs::read_dir(root).with_context(|| format!("reading {}", root.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            collect_source_files(&path, out)?;
        } else if ft.is_file() {
            if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                if matches!(ext, "rs" | "py" | "ts" | "tsx" | "js" | "jsx") {
                    out.push(path);
                }
            }
        }
    }
    Ok(())
}

/// Watch the given `root` directory for changes, extracting and re-embedding
/// symbols for changed supported source files dynamically.
#[cfg(feature = "semantic")]
pub async fn watch_and_reindex(index: &mut SemanticIndex, root: &Path) {
    use notify::{Event, RecursiveMode, Watcher};
    use std::collections::HashSet;
    use std::time::Duration;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            let _ = tx.send(event);
        }
    })
    .expect("failed to create watcher");

    watcher
        .watch(root, RecursiveMode::Recursive)
        .expect("failed to watch");
    let embedder = Embedder::new().expect("failed to load embedder");

    loop {
        let mut events = Vec::new();
        match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            Ok(Some(e)) => events.push(e),
            Ok(None) => break,
            Err(_) => continue,
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }

        let mut changed_files = HashSet::new();
        for event in events {
            for path in event.paths {
                let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                if matches!(ext, "rs" | "py" | "ts" | "tsx" | "js" | "jsx") {
                    changed_files.insert(path);
                }
            }
        }

        for file in changed_files {
            let Ok(content) = std::fs::read_to_string(&file) else {
                index.remove_by_file(&file);
                continue;
            };

            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&content, &mut hasher);
            let hash = std::hash::Hasher::finish(&hasher);

            if index.file_hashes.get(&file) == Some(&hash) {
                continue;
            }

            index.remove_by_file(&file);
            index.file_hashes.insert(file.clone(), hash);

            let slices = extract_symbols(&content, &file);
            for chunk in slices.chunks(32) {
                let texts: Vec<&str> = chunk.iter().map(|s| s.signature.as_str()).collect();
                if let Ok(vecs) = embedder.embed(&texts) {
                    for (slice, vec) in chunk.iter().cloned().zip(vecs.into_iter()) {
                        let _ = index.add(slice, vec);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "semantic")]
    use phonton_types::{CodeSlice, SliceOrigin};

    #[cfg(feature = "semantic")]
    fn synth(name: &str, sig: &str) -> CodeSlice {
        CodeSlice {
            file_path: PathBuf::from(format!("{name}.rs")),
            symbol_name: name.to_string(),
            signature: sig.to_string(),
            docstring: None,
            callsites: Vec::new(),
            token_count: 0,
            origin: SliceOrigin::Semantic,
        }
    }

    // -------------------------------------------------------------
    // Nexus config tests (no embedder required)
    // -------------------------------------------------------------

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn load_nexus_config_parses_minimal_json() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = write(
            tmp.path(),
            "nexus.json",
            r#"{
                "version": 1,
                "repos": [
                    { "name": "phonton-types", "path": "../types-tree" },
                    { "name": "shared-libs",   "path": "../shared" }
                ]
            }"#,
        );
        let cfg = load_nexus_config(&cfg_path).unwrap();
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.repos.len(), 2);
        assert_eq!(cfg.repos[0].name, "phonton-types");
        assert_eq!(cfg.config_dir, tmp.path());
    }

    #[test]
    fn load_nexus_config_tolerates_unknown_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = write(
            tmp.path(),
            "nexus.json",
            r#"{ "version": 1, "repos": [], "future_key": 42 }"#,
        );
        let cfg = load_nexus_config(&cfg_path).unwrap();
        assert!(cfg.repos.is_empty());
    }

    #[test]
    fn discover_nexus_config_walks_upward() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "nexus.json", r#"{ "version": 1, "repos": [] }"#);
        let nested = tmp.path().join("phonton-cli/src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        let found = discover_nexus_config(&nested).unwrap();
        assert!(found.is_some(), "should walk up to find nexus.json");
        assert_eq!(found.unwrap().config_dir, tmp.path());
    }

    #[test]
    fn discover_nexus_config_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let found = discover_nexus_config(tmp.path()).unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn resolve_handles_relative_and_absolute_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = NexusConfig {
            version: 1,
            repos: vec![
                NexusRepo {
                    name: "rel".into(),
                    path: PathBuf::from("../sibling"),
                },
                NexusRepo {
                    name: "abs".into(),
                    path: PathBuf::from(if cfg!(windows) {
                        "C:\\elsewhere"
                    } else {
                        "/elsewhere"
                    }),
                },
            ],
            config_dir: tmp.path().to_path_buf(),
        };
        let pairs = cfg.resolved_repos();
        assert_eq!(pairs[0].1, tmp.path().join("../sibling"));
        // Absolute path passes through unchanged.
        assert!(pairs[1].1.is_absolute());
    }

    #[test]
    fn collect_source_files_respects_skip_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "src/lib.rs", "pub fn a() {}");
        write(tmp.path(), "target/junk.rs", "fn skip_me() {}");
        write(tmp.path(), "node_modules/x.js", "function skip(){}");
        write(tmp.path(), ".hidden/x.rs", "fn skip(){}");

        let mut out = Vec::new();
        collect_source_files(tmp.path(), &mut out).unwrap();
        assert_eq!(out.len(), 1, "expected only src/lib.rs, got {out:?}");
        assert!(out[0].ends_with("lib.rs"));
    }

    #[test]
    fn extracted_symbols_have_token_counts() {
        let slices = extract_symbols(
            "pub fn save_tokens(input: &str) -> usize { input.len() }",
            Path::new("src/lib.rs"),
        );
        assert!(!slices.is_empty());
        assert!(
            slices.iter().all(|s| s.token_count > 0),
            "all extracted slices should carry a non-zero token estimate: {slices:?}"
        );
    }

    #[cfg(feature = "semantic")]
    #[test]
    #[ignore = "downloads ~90MB ONNX model on first run"]
    fn semantic_search_ranks_related_first() {
        let embedder = Embedder::new().expect("load model");
        let mut index = SemanticIndex::new(EMBED_DIM);

        let slices = vec![
            synth(
                "authenticate_user",
                "fn authenticate_user(username: &str, password: &str) -> Result<Session>",
            ),
            synth(
                "render_image",
                "fn render_image(pixels: &[u8], width: u32, height: u32) -> Png",
            ),
            synth(
                "parse_json",
                "fn parse_json(input: &str) -> Result<serde_json::Value>",
            ),
        ];

        let texts: Vec<&str> = slices.iter().map(|s| s.signature.as_str()).collect();
        let vecs = embedder.embed(&texts).expect("embed");
        for (s, v) in slices.into_iter().zip(vecs.into_iter()) {
            index.add(s, v).unwrap();
        }

        let q = embedder
            .embed(&["log a user in with their credentials"])
            .unwrap()
            .pop()
            .unwrap();
        let top = index.search(&q, 3);
        assert!(!top.is_empty());
        assert_eq!(top[0].symbol_name, "authenticate_user");
    }
}
