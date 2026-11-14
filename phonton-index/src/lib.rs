//! Semantic code index: tree-sitter parsing + heuristic fallbacks +
//! HNSW-based vector retrieval over extracted symbols.

use anyhow::{Context, Result};
#[cfg(feature = "semantic")]
use async_trait::async_trait;
use phonton_types::{CodeSlice, SliceOrigin};
use regex::Regex;
use serde::{Deserialize, Serialize};
#[cfg(feature = "semantic")]
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
#[cfg(feature = "semantic")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "semantic")]
use std::sync::{Arc, OnceLock};
#[cfg(feature = "semantic")]
use std::time::{Duration, Instant};

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
    "class_declaration",
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
        "ts" | "js" | "mts" | "cts" | "mjs" | "cjs" | "tsx" | "jsx" => extract_semantic(
            source,
            file_path,
            tree_sitter_typescript::language_typescript(),
            TS_KINDS,
        ),
        _ => extract_fallback(source, file_path),
    }
}

/// Extract exact inclusive line spans without embeddings, downloads or network.
/// The traversal is bounded; unsupported languages return no parsed spans so
/// callers can explicitly use lexical source windows instead.
pub fn extract_symbol_spans(
    source: &str,
    file_path: &Path,
) -> Vec<phonton_types::code_context::SymbolSpan> {
    let extension = file_path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let (language, kinds) = match extension {
        "rs" => (tree_sitter_rust::language(), RUST_KINDS),
        "py" => (tree_sitter_python::language(), PYTHON_KINDS),
        "ts" | "js" | "mts" | "cts" | "mjs" | "cjs" => {
            (tree_sitter_typescript::language_typescript(), TS_KINDS)
        }
        "tsx" | "jsx" => (tree_sitter_typescript::language_tsx(), TS_KINDS),
        _ => return Vec::new(),
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut stack = vec![tree.root_node()];
    let mut spans = Vec::new();
    let mut visited = 0;
    while let Some(node) = stack.pop() {
        visited += 1;
        if visited > 200_000 || spans.len() >= 4096 {
            break;
        }
        if kinds.contains(&node.kind()) || node.kind() == "variable_declarator" {
            if let Some(name) = extract_name(node, source.as_bytes()) {
                spans.push(phonton_types::code_context::SymbolSpan {
                    name,
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + usize::from(node.end_position().column > 0),
                });
            }
        }
        for index in (0..node.child_count()).rev() {
            if let Some(child) = node.child(index) {
                stack.push(child);
            }
        }
    }
    spans
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
        let signature = bounded_utf8(&source[start..], 200).to_string();

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
    bounded_utf8(text, 400).to_string()
}

fn bounded_utf8(text: &str, maximum: usize) -> &str {
    let mut end = maximum.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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
    /// Where vectors made by this model are kept between runs.
    vectors: PathBuf,
}

#[cfg(feature = "semantic")]
impl Embedder {
    /// Load `all-MiniLM-L6-v2` outside the current working directory.
    pub fn new() -> Result<Self> {
        let cwd = std::env::current_dir().context("locating embedding workspace")?;
        Self::new_for_workspace(&cwd)
    }

    /// Load `all-MiniLM-L6-v2` with its cache outside the indexed workspace.
    pub fn new_for_workspace(workspace: &Path) -> Result<Self> {
        let cache_dir = embedding_cache_dir(
            workspace,
            dirs::cache_dir(),
            std::env::var_os("FASTEMBED_CACHE_DIR"),
            std::env::var_os("HF_HOME"),
        )?;
        let vectors = cache_dir.join("vectors");
        let model = fastembed::TextEmbedding::try_new(
            fastembed::InitOptions::new(fastembed::EmbeddingModel::AllMiniLML6V2)
                .with_cache_dir(cache_dir)
                .with_show_download_progress(false),
        )
        .context("loading all-MiniLM-L6-v2")?;
        Ok(Self { model, vectors })
    }

    /// The vector cache file of one workspace.
    fn vector_file(&self, root: &Path) -> PathBuf {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        self.vectors
            .join(format!("{:016x}.bin", text_key(&root.to_string_lossy())))
    }

    /// Embed a batch of texts. Returns one 384-dim vector per input.
    pub fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let docs: Vec<&str> = texts.to_vec();
        // One text per model run, with the runs spread over the cores. As one
        // batch the texts are padded to the longest and run as a single large
        // tensor: the same vectors for 4.3 times the CPU time and twice the
        // memory (512 symbols: 237 CPU-seconds and 385 MB against 55 and 205).
        self.model
            .embed(docs, Some(1))
            .map_err(|e| anyhow::anyhow!("fastembed: {e}"))
    }
}

#[cfg(feature = "semantic")]
fn embedding_cache_dir(
    workspace: &Path,
    user_cache: Option<PathBuf>,
    fastembed_override: Option<std::ffi::OsString>,
    hf_home: Option<std::ffi::OsString>,
) -> Result<PathBuf> {
    let configured = match fastembed_override {
        Some(path) => PathBuf::from(path),
        None => user_cache
            .ok_or_else(|| anyhow::anyhow!("cannot locate a user model cache directory"))?
            .join("Phonton")
            .join("fastembed"),
    };
    let cache = safe_embedding_cache_path(&configured, workspace)?;
    // fastembed 4.x gives HF_HOME precedence over InitOptions.cache_dir.
    if let Some(path) = hf_home {
        safe_embedding_cache_path(&PathBuf::from(path), workspace)?;
    }
    Ok(cache)
}

#[cfg(feature = "semantic")]
fn safe_embedding_cache_path(path: &Path, workspace: &Path) -> Result<PathBuf> {
    use std::path::Component;

    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        anyhow::bail!("embedding cache location must be an absolute path without parent traversal");
    }
    let project = std::fs::canonicalize(workspace).context("resolving embedding workspace")?;
    let mut ancestor = path;
    let mut missing = Vec::new();
    while !ancestor.exists() {
        let name = ancestor
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("embedding cache location has no existing ancestor"))?;
        missing.push(name.to_os_string());
        ancestor = ancestor
            .parent()
            .ok_or_else(|| anyhow::anyhow!("embedding cache location has no parent"))?;
    }
    let mut resolved =
        std::fs::canonicalize(ancestor).context("resolving embedding cache location")?;
    for name in missing.into_iter().rev() {
        resolved.push(name);
    }
    if resolved.starts_with(&project) {
        anyhow::bail!("embedding cache location is inside the working repository");
    }
    Ok(resolved)
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
// Vector cache
// ---------------------------------------------------------------------------

/// First bytes of a vector cache file. Vectors are only valid for the model
/// that made them: change this when the embedding model changes.
#[cfg(feature = "semantic")]
const VECTOR_CACHE_MAGIC: &[u8; 8] = b"PHVEC001";

/// One record of a vector cache file: the key, then the vector.
#[cfg(feature = "semantic")]
const VECTOR_RECORD_BYTES: usize = 8 + EMBED_DIM * 4;

/// Seconds of embedding between two saves of an unfinished build.
#[cfg(feature = "semantic")]
const VECTOR_SAVE_INTERVAL: Duration = Duration::from_secs(10);

#[cfg(feature = "semantic")]
fn text_key(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(text, &mut hasher);
    std::hash::Hasher::finish(&hasher)
}

/// Read a vector cache. A missing, foreign or cut-off file yields the whole
/// records it holds, which may be none.
#[cfg(feature = "semantic")]
fn load_vectors(path: &Path) -> HashMap<u64, Vec<f32>> {
    let Ok(bytes) = std::fs::read(path) else {
        return HashMap::new();
    };
    let Some(records) = bytes.strip_prefix(VECTOR_CACHE_MAGIC) else {
        return HashMap::new();
    };
    records
        .as_chunks::<VECTOR_RECORD_BYTES>()
        .0
        .iter()
        .filter_map(|record| {
            let (key, values) = record.split_first_chunk::<8>()?;
            let vector = values
                .as_chunks::<4>()
                .0
                .iter()
                .map(|v| f32::from_le_bytes(*v))
                .collect();
            Some((u64::from_le_bytes(*key), vector))
        })
        .collect()
}

/// Write the vectors whose key is in `live`, replacing the file in one step.
#[cfg(feature = "semantic")]
fn save_vectors(
    path: &Path,
    vectors: &HashMap<u64, Vec<f32>>,
    live: &HashSet<u64>,
) -> std::io::Result<()> {
    let mut bytes = Vec::with_capacity(8 + live.len() * VECTOR_RECORD_BYTES);
    bytes.extend_from_slice(VECTOR_CACHE_MAGIC);
    for (key, vector) in vectors.iter().filter(|(key, _)| live.contains(key)) {
        bytes.extend_from_slice(&key.to_le_bytes());
        for value in vector {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder)?;
    }
    // Written beside the target and renamed over it, so another Phonton
    // process reading the cache never sees half a file.
    let partial = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&partial, bytes)?;
    std::fs::rename(&partial, path)
}

/// Give every slice its vector and add it to `index`.
///
/// A vector depends only on the signature text, and embedding costs about
/// 0.1 CPU-seconds per symbol, so vectors are kept in `cache_file` and each
/// text is embedded once. `stop` is checked between batches. What was embedded
/// is saved even when the build stops early or fails, so the next build
/// continues from there. Slices left without a vector are not indexed.
#[cfg(feature = "semantic")]
fn add_embedded(
    index: &mut SemanticIndex,
    slices: Vec<CodeSlice>,
    cache_file: &Path,
    stop: &AtomicBool,
    embed: impl Fn(&[&str]) -> Result<Vec<Vec<f32>>>,
) -> Result<()> {
    let mut vectors = load_vectors(cache_file);
    let keys: Vec<u64> = slices.iter().map(|s| text_key(&s.signature)).collect();
    let live: HashSet<u64> = keys.iter().copied().collect();
    let stale = vectors.keys().any(|key| !live.contains(key));
    let mut queued = HashSet::new();
    let missing: Vec<(u64, &str)> = keys
        .iter()
        .zip(&slices)
        .filter(|(key, _)| !vectors.contains_key(key) && queued.insert(**key))
        .map(|(key, slice)| (*key, slice.signature.as_str()))
        .collect();

    let mut outcome = Ok(());
    let mut saved = Instant::now();
    for chunk in missing.chunks(32) {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let texts: Vec<&str> = chunk.iter().map(|(_, text)| *text).collect();
        match embed(&texts) {
            Ok(made) => vectors.extend(chunk.iter().map(|(key, _)| *key).zip(made)),
            Err(error) => {
                outcome = Err(error);
                break;
            }
        }
        if saved.elapsed() >= VECTOR_SAVE_INTERVAL {
            let _ = save_vectors(cache_file, &vectors, &live);
            saved = Instant::now();
        }
    }
    if stale || !missing.is_empty() {
        // A cache that cannot be written costs time on the next build, not
        // correctness, so the index is still returned.
        let _ = save_vectors(cache_file, &vectors, &live);
    }
    outcome?;

    for (key, slice) in keys.into_iter().zip(slices) {
        if let Some(vector) = vectors.get(&key) {
            index.add(slice, vector.clone())?;
        }
    }
    Ok(())
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
    let embedder = Embedder::new_for_workspace(root)?;
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
    index_nexus(root, config, embedder, &AtomicBool::new(false))
}

#[cfg(feature = "semantic")]
fn index_nexus(
    root: &Path,
    config: &NexusConfig,
    embedder: &Embedder,
    stop: &AtomicBool,
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

    let cache_file = embedder.vector_file(root);
    add_embedded(&mut index, all_slices, &cache_file, stop, |texts| {
        embedder.embed(texts)
    })?;
    Ok(index)
}

/// Walk `root`, extract symbols from all Rust/Python/TypeScript sources,
/// embed the signatures the vector cache does not hold yet in batches of 32,
/// and return an HNSW index.
#[cfg(feature = "semantic")]
pub async fn index_workspace(root: &Path) -> Result<SemanticIndex> {
    let embedder = Embedder::new_for_workspace(root)?;
    index_workspace_using_embedder(root, &embedder).await
}

/// Build a single-workspace index using a caller-owned embedder.
#[cfg(feature = "semantic")]
pub async fn index_workspace_using_embedder(
    root: &Path,
    embedder: &Embedder,
) -> Result<SemanticIndex> {
    index_single(root, embedder, &AtomicBool::new(false))
}

#[cfg(feature = "semantic")]
fn index_single(root: &Path, embedder: &Embedder, stop: &AtomicBool) -> Result<SemanticIndex> {
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

    let cache_file = embedder.vector_file(root);
    add_embedded(&mut index, all_slices, &cache_file, stop, |texts| {
        embedder.embed(texts)
    })?;
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

/// Common async retrieval interface used by workers regardless of backend.
#[cfg(feature = "semantic")]
#[async_trait]
pub trait CodeRetriever: Send + Sync {
    /// Stable backend label for diagnostics and plan evidence.
    fn backend_name(&self) -> &str;

    /// Return the top relevant code slices for `query`.
    async fn query(&self, query: &str, k: usize) -> Result<Vec<CodeSlice>>;
}

/// Local HNSW retriever backed by fastembed + usearch.
#[cfg(feature = "semantic")]
pub struct LocalCodeRetriever {
    embedder: Embedder,
    index: SemanticIndex,
}

#[cfg(feature = "semantic")]
impl LocalCodeRetriever {
    /// Build a local retriever from caller-owned pieces.
    pub fn new(embedder: Embedder, index: SemanticIndex) -> Self {
        Self { embedder, index }
    }
}

#[cfg(feature = "semantic")]
#[async_trait]
impl CodeRetriever for LocalCodeRetriever {
    fn backend_name(&self) -> &str {
        "local-hnsw"
    }

    async fn query(&self, query: &str, k: usize) -> Result<Vec<CodeSlice>> {
        Ok(query_relevant_slices(&self.index, &self.embedder, query, k).await)
    }
}

/// How long the first query waits for an index that is still being built.
/// With every vector in the cache the build takes a second or two. Longer
/// than this means symbols are being embedded, which can take minutes.
#[cfg(feature = "semantic")]
const FIRST_QUERY_WAIT: Duration = Duration::from_secs(10);

/// How long dropping the retriever waits for the build thread to end. A batch
/// takes under a second. A first run that is still downloading the model
/// is not worth holding the caller for, and gets no more than this.
#[cfg(feature = "semantic")]
const DROP_WAIT: Duration = Duration::from_secs(3);

/// The local retriever, built on its own thread.
///
/// The first index of a repository embeds every symbol, which takes minutes of
/// CPU time, and a goal should not wait for that. The first query waits
/// [`FIRST_QUERY_WAIT`] at most; until the index is ready, queries return
/// nothing, the same as a goal that has no index. Dropping the retriever stops
/// the build after the batch in flight and waits for the thread to end, so no
/// model code is running if the process exits next. The vectors made so far
/// stay in the cache for the next goal.
#[cfg(feature = "semantic")]
pub struct BackgroundCodeRetriever {
    ready: Arc<OnceLock<LocalCodeRetriever>>,
    stop: Arc<AtomicBool>,
    /// Closes when the build thread ends, ready or not.
    finished: tokio::sync::watch::Receiver<()>,
    thread: std::thread::JoinHandle<()>,
    wait: Duration,
    waited: AtomicBool,
}

#[cfg(feature = "semantic")]
impl BackgroundCodeRetriever {
    /// Start indexing `root`. `on_error` runs on the build thread if the
    /// model cannot be loaded or the workspace cannot be indexed.
    pub fn spawn(
        root: PathBuf,
        on_error: impl FnOnce(anyhow::Error) + Send + 'static,
    ) -> std::io::Result<Self> {
        let build = move |stop: &AtomicBool| {
            let embedder = Embedder::new_for_workspace(&root)?;
            let index = match discover_nexus_config(&root)? {
                Some(config) => index_nexus(&root, &config, &embedder, stop)?,
                None => index_single(&root, &embedder, stop)?,
            };
            Ok(LocalCodeRetriever::new(embedder, index))
        };
        Self::start(build, on_error, FIRST_QUERY_WAIT)
    }

    fn start(
        build: impl FnOnce(&AtomicBool) -> Result<LocalCodeRetriever> + Send + 'static,
        on_error: impl FnOnce(anyhow::Error) + Send + 'static,
        wait: Duration,
    ) -> std::io::Result<Self> {
        let ready = Arc::new(OnceLock::new());
        let stop = Arc::new(AtomicBool::new(false));
        let (done, finished) = tokio::sync::watch::channel(());
        let (slot, flag) = (Arc::clone(&ready), Arc::clone(&stop));
        // A thread of its own, not `spawn_blocking`: a runtime waits for its
        // blocking tasks when it shuts down, and the process should not. The
        // stack matches the runtime's threads, which ran this work before.
        let thread = std::thread::Builder::new()
            .name("phonton-index".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let _done = done;
                match build(&flag) {
                    // A stopped build leaves symbols out, so it is not used.
                    Ok(retriever) if !flag.load(Ordering::Relaxed) => {
                        let _ = slot.set(retriever);
                    }
                    Ok(_) => {}
                    Err(error) => on_error(error),
                }
            })?;
        Ok(Self {
            ready,
            stop,
            finished,
            thread,
            wait,
            waited: AtomicBool::new(false),
        })
    }
}

#[cfg(feature = "semantic")]
#[async_trait]
impl CodeRetriever for BackgroundCodeRetriever {
    fn backend_name(&self) -> &str {
        "local-hnsw"
    }

    async fn query(&self, query: &str, k: usize) -> Result<Vec<CodeSlice>> {
        if self.ready.get().is_none() && !self.waited.load(Ordering::Relaxed) {
            let mut finished = self.finished.clone();
            if tokio::time::timeout(self.wait, finished.changed())
                .await
                .is_err()
            {
                self.waited.store(true, Ordering::Relaxed);
            }
        }
        match self.ready.get() {
            Some(retriever) => retriever.query(query, k).await,
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(feature = "semantic")]
impl Drop for BackgroundCodeRetriever {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + DROP_WAIT;
        while !self.thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Qdrant HTTP backend configuration.
#[cfg(feature = "semantic")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QdrantConfig {
    /// Base URL with trailing slash removed.
    pub base_url: String,
    /// Collection containing Phonton code slices.
    pub collection: String,
}

#[cfg(feature = "semantic")]
impl QdrantConfig {
    /// Create a config, normalizing the base URL for request construction.
    pub fn new(base_url: impl Into<String>, collection: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            collection: collection.into(),
        }
    }
}

/// Qdrant-backed code retriever. Phonton owns embeddings; Qdrant stores vectors.
#[cfg(feature = "semantic")]
pub struct QdrantCodeRetriever {
    cfg: QdrantConfig,
    embedder: Embedder,
    http: reqwest::Client,
}

#[cfg(feature = "semantic")]
impl QdrantCodeRetriever {
    /// Build a retriever for an existing Qdrant collection.
    pub fn new(cfg: QdrantConfig, embedder: Embedder) -> Self {
        Self {
            cfg,
            embedder,
            http: reqwest::Client::new(),
        }
    }

    fn search_url(&self) -> String {
        format!(
            "{}/collections/{}/points/search",
            self.cfg.base_url, self.cfg.collection
        )
    }
}

#[cfg(feature = "semantic")]
#[async_trait]
impl CodeRetriever for QdrantCodeRetriever {
    fn backend_name(&self) -> &str {
        "qdrant"
    }

    async fn query(&self, query: &str, k: usize) -> Result<Vec<CodeSlice>> {
        let vector = self
            .embedder
            .embed(&[query])?
            .into_iter()
            .next()
            .unwrap_or_default();
        query_qdrant_search(&self.http, self.search_url(), vector, k).await
    }
}

#[cfg(feature = "semantic")]
async fn query_qdrant_search(
    http: &reqwest::Client,
    search_url: String,
    vector: Vec<f32>,
    k: usize,
) -> Result<Vec<CodeSlice>> {
    let body = serde_json::json!({
        "vector": vector,
        "limit": k,
        "with_payload": true
    });
    let value: serde_json::Value = http
        .post(search_url)
        .json(&body)
        .send()
        .await
        .context("querying qdrant")?
        .error_for_status()
        .context("qdrant returned error status")?
        .json()
        .await
        .context("parsing qdrant search response")?;

    let mut out = Vec::new();
    if let Some(rows) = value.get("result").and_then(|v| v.as_array()) {
        for row in rows {
            if let Some(payload) = row.get("payload") {
                if let Ok(slice) = serde_json::from_value::<CodeSlice>(payload.clone()) {
                    out.push(slice);
                }
            }
        }
    }
    Ok(out)
}

/// True for the files the index reads: every extension [`extract_symbols`]
/// parses, so a `.mjs` or `.cts` module is found like a `.js` or `.ts` one.
#[cfg(any(feature = "semantic", test))]
fn is_indexed_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("rs" | "py" | "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs")
    )
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
        } else if ft.is_file() && is_indexed_source(&path) {
            out.push(path);
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
    let embedder = Embedder::new_for_workspace(root).expect("failed to load embedder");

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
                if is_indexed_source(&path) {
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
                    for (slice, vec) in chunk.iter().cloned().zip(vecs) {
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
    #[test]
    fn embedding_cache_stays_outside_workspace_and_refuses_unsafe_overrides() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let workspace = temp.path().join("repo");
        let user_cache = temp.path().join("user-cache");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&user_cache)?;
        let resolved_user_cache = std::fs::canonicalize(&user_cache)?;

        let chosen = embedding_cache_dir(&workspace, Some(user_cache.clone()), None, None)?;
        assert_eq!(
            chosen,
            resolved_user_cache.join("Phonton").join("fastembed")
        );
        assert!(!workspace.join(".fastembed_cache").exists());
        assert!(embedding_cache_dir(
            &workspace,
            Some(user_cache.clone()),
            Some(".fastembed_cache".into()),
            None
        )
        .is_err());
        assert!(embedding_cache_dir(
            &workspace,
            Some(user_cache.clone()),
            Some(workspace.join(".fastembed_cache").into_os_string()),
            None
        )
        .is_err());
        assert!(embedding_cache_dir(
            &workspace,
            Some(user_cache.clone()),
            None,
            Some("relative-hf-cache".into())
        )
        .is_err());
        assert!(embedding_cache_dir(
            &workspace,
            Some(user_cache.clone()),
            None,
            Some(workspace.join("hf-cache").into_os_string())
        )
        .is_err());
        assert_eq!(
            embedding_cache_dir(
                &workspace,
                Some(user_cache.clone()),
                Some(user_cache.clone().into_os_string()),
                None
            )?,
            resolved_user_cache
        );
        Ok(())
    }

    #[test]
    fn source_extraction_never_splits_utf8() {
        let source = format!("pub fn sample() {{\n    // {}\n}}\n", "é".repeat(250));
        assert!(!source.is_char_boundary(400));
        let slices = extract_symbols(&source, Path::new("sample.rs"));
        assert_eq!(slices.len(), 1);
        assert!(slices[0].signature.len() <= 400);
        let fallback = format!("function sample()  {}\n", "é".repeat(150));
        assert!(!fallback.is_char_boundary(200));
        let slices = extract_symbols(&fallback, Path::new("sample.txt"));
        assert_eq!(slices.len(), 1);
        assert!(slices[0].signature.len() <= 200);
    }

    #[test]
    fn parsed_spans_identify_complete_functions_and_tsx_methods() {
        let spans = extract_symbol_spans(
            "// header\nexport function sum(a, b) {\n  return a + b;\n}\n",
            Path::new("sum.js"),
        );
        assert!(spans
            .iter()
            .any(|s| s.name == "sum" && s.start_line == 2 && s.end_line == 4));
        let spans = extract_symbol_spans(
            "export function View() {\n  return <main>hello</main>;\n}\n",
            Path::new("View.tsx"),
        );
        assert!(spans.iter().any(|s| s.name == "View" && s.end_line == 3));
    }
    #[test]
    fn module_extensions_keep_complete_function_spans() {
        let source = "export function sumInclusive(start, end) {\n  return start + end;\n}\n";
        for extension in ["mjs", "cjs", "mts", "cts"] {
            let path = PathBuf::from(format!("sum.{extension}"));
            assert!(extract_symbol_spans(source, &path)
                .iter()
                .any(|s| s.name == "sumInclusive" && s.start_line == 1 && s.end_line == 3));
            assert!(extract_symbols(source, &path)
                .iter()
                .any(|s| s.symbol_name == "sumInclusive"));
        }
    }
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

    #[cfg(feature = "semantic")]
    #[test]
    fn qdrant_config_trims_url_and_keeps_collection() {
        let cfg = QdrantConfig::new("http://127.0.0.1:6333/", "phonton-code");
        assert_eq!(cfg.base_url, "http://127.0.0.1:6333");
        assert_eq!(cfg.collection, "phonton-code");
    }

    #[cfg(feature = "semantic")]
    #[tokio::test]
    async fn qdrant_search_posts_vector_and_decodes_payload() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let expected = synth("search_code", "pub fn search_code() {}");
        let response_body = serde_json::json!({
            "result": [
                { "id": 1, "score": 0.99, "payload": expected }
            ]
        })
        .to_string();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            loop {
                let read = stream.read(&mut buf).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
                let text = String::from_utf8_lossy(&request);
                if text.contains("\"with_payload\"") {
                    break;
                }
            }
            let text = String::from_utf8_lossy(&request);
            assert!(text.contains("POST /collections/phonton-code/points/search HTTP/1.1"));
            assert!(text.contains("\"limit\":2"));
            assert!(text.contains("\"vector\""));
            assert!(text.contains("\"with_payload\":true"));

            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let slices = query_qdrant_search(
            &reqwest::Client::new(),
            format!("http://{addr}/collections/phonton-code/points/search"),
            vec![0.1, 0.2],
            2,
        )
        .await
        .unwrap();
        server.await.unwrap();

        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].symbol_name, "search_code");
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
    fn collect_source_files_finds_every_parsed_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let parsed = [
            "rs", "py", "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs",
        ];
        for extension in parsed {
            write(tmp.path(), &format!("src/rules.{extension}"), "");
        }
        write(tmp.path(), "README.md", "");
        write(tmp.path(), "data.json", "{}");

        let mut out = Vec::new();
        collect_source_files(tmp.path(), &mut out).unwrap();
        let mut found: Vec<&str> = out
            .iter()
            .filter_map(|path| path.extension()?.to_str())
            .collect();
        found.sort_unstable();
        let mut expected = parsed.to_vec();
        expected.sort_unstable();
        assert_eq!(found, expected);
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

    /// A stand-in for the model: one fixed vector per text length.
    #[cfg(feature = "semantic")]
    fn fake_vectors(texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| {
                let mut vector = vec![0.0; EMBED_DIM];
                vector[text.len() % EMBED_DIM] = 1.0;
                vector
            })
            .collect())
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn each_signature_is_embedded_once_and_reused_by_later_builds() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = temp.path().join("vectors").join("repo.bin");
        let never = AtomicBool::new(false);
        let embedded = std::cell::RefCell::new(Vec::new());
        let embed = |texts: &[&str]| {
            embedded
                .borrow_mut()
                .extend(texts.iter().map(|text| text.to_string()));
            fake_vectors(texts)
        };
        let build = |signatures: &[&str]| -> Result<SemanticIndex> {
            let mut index = SemanticIndex::new(EMBED_DIM);
            let slices = signatures.iter().map(|s| synth("f", s)).collect();
            add_embedded(&mut index, slices, &cache, &never, embed)?;
            Ok(index)
        };

        // The same text twice is one embedding and two indexed slices.
        let first = build(&["fn a()", "fn bb()", "fn a()"])?;
        assert_eq!(first.len(), 3);
        assert_eq!(*embedded.borrow(), ["fn a()", "fn bb()"]);

        // Nothing changed: the model is not called, and search still works.
        embedded.borrow_mut().clear();
        let second = build(&["fn a()", "fn bb()", "fn a()"])?;
        assert_eq!(second.len(), 3);
        assert!(embedded.borrow().is_empty());
        let query = fake_vectors(&["fn bb()"])?.remove(0);
        assert_eq!(second.search(&query, 1)[0].signature, "fn bb()");

        // One signature changed: one embedding, and the old text leaves the file.
        let third = build(&["fn a()", "fn ccc()"])?;
        assert_eq!(third.len(), 2);
        assert_eq!(*embedded.borrow(), ["fn ccc()"]);
        assert_eq!(load_vectors(&cache).len(), 2);

        // A cut-off file costs only the record that was cut.
        let mut bytes = std::fs::read(&cache)?;
        bytes.truncate(bytes.len() - 5);
        std::fs::write(&cache, bytes)?;
        assert_eq!(load_vectors(&cache).len(), 1);
        Ok(())
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn a_stopped_build_keeps_the_vectors_it_made() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = temp.path().join("repo.bin");
        let signatures: Vec<String> = (0..40).map(|i| format!("fn f{i}()")).collect();
        let slices = || signatures.iter().map(|s| synth("f", s)).collect::<Vec<_>>();

        // Stop is raised during the first batch of 32, so the second never runs.
        let stop = AtomicBool::new(false);
        let mut index = SemanticIndex::new(EMBED_DIM);
        add_embedded(&mut index, slices(), &cache, &stop, |texts| {
            stop.store(true, Ordering::Relaxed);
            fake_vectors(texts)
        })?;
        assert_eq!(index.len(), 32);
        assert_eq!(load_vectors(&cache).len(), 32);

        // The next build embeds only the eight that are left.
        let embedded = std::cell::Cell::new(0);
        let mut index = SemanticIndex::new(EMBED_DIM);
        let never = AtomicBool::new(false);
        add_embedded(&mut index, slices(), &cache, &never, |texts| {
            embedded.set(embedded.get() + texts.len());
            fake_vectors(texts)
        })?;
        assert_eq!((index.len(), embedded.get()), (40, 8));
        Ok(())
    }

    #[cfg(feature = "semantic")]
    #[tokio::test]
    async fn queries_do_not_wait_for_a_failed_or_slow_index_build() -> Result<()> {
        // A build that fails: the error is reported and queries return nothing.
        let (reported, report) = std::sync::mpsc::channel();
        let failed = BackgroundCodeRetriever::start(
            |_| anyhow::bail!("no model"),
            move |error| {
                let _ = reported.send(error.to_string());
            },
            Duration::from_secs(60),
        )?;
        assert!(failed.query("anything", 5).await?.is_empty());
        assert_eq!(report.recv()?, "no model");

        // A build that is still running: the first query waits `wait` and no
        // longer, later ones do not wait, and dropping the retriever stops it
        // and returns once the thread has ended.
        let (stopped, stop_seen) = std::sync::mpsc::channel();
        let slow = BackgroundCodeRetriever::start(
            move |stop| {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let _ = stopped.send(());
                anyhow::bail!("stopped")
            },
            |_| {},
            Duration::from_millis(50),
        )?;
        let asked = Instant::now();
        assert!(slow.query("anything", 5).await?.is_empty());
        assert!(asked.elapsed() >= Duration::from_millis(50));
        assert!(slow.waited.load(Ordering::Relaxed));
        assert!(slow.query("anything", 5).await?.is_empty());
        drop(slow);
        stop_seen.try_recv()?;
        Ok(())
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
        for (s, v) in slices.into_iter().zip(vecs) {
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
