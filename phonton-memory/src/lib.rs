//! Cross-session memory facade over [`phonton_store::Store`].
//!
//! Workers call [`MemoryStore::record`] after a subtask completes to log
//! a `Decision`, `Constraint`, `RejectedApproach`, or `Convention`. The
//! planner calls [`MemoryStore::query`] before decomposing a new goal to
//! surface prior context relevant to the request.
//!
//! Relevance at this layer is **semantic retrieval via HNSW index** with
//! keyword-overlap fallback.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use phonton_store::{MemoryEntry, Store};
use phonton_types::MemoryRecord;

/// Thread-safe singleton for lazy fastembed model loading.
pub struct Embedder {
    model: TextEmbedding,
}

impl Embedder {
    /// Retrieve or initialize the shared fastembed model.
    pub fn get() -> Result<&'static Self> {
        static INSTANCE: OnceLock<Result<Embedder, String>> = OnceLock::new();
        let res = INSTANCE.get_or_init(|| {
            TextEmbedding::try_new(InitOptions::new(EmbeddingModel::AllMiniLML6V2))
                .map(|model| Embedder { model })
                .map_err(|e| format!("failed to initialize fastembed: {e}"))
        });
        res.as_ref().map_err(|e| anyhow::anyhow!("{}", e))
    }

    /// Embed a batch of texts.
    pub fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let docs: Vec<&str> = texts.to_vec();
        self.model
            .embed(docs, None)
            .map_err(|e| anyhow::anyhow!("fastembed error: {e}"))
    }
}

/// Rebuild the HNSW index from scratch using all memories in the store.
fn rebuild_hnsw_index_sync(store: &Store, hnsw: &mut Option<usearch::Index>) -> Result<()> {
    let options = usearch::IndexOptions {
        dimensions: 384,
        metric: usearch::MetricKind::Cos,
        quantization: usearch::ScalarKind::F32,
        ..Default::default()
    };
    let index =
        usearch::new_index(&options).map_err(|e| anyhow::anyhow!("usearch::new_index: {e}"))?;

    let entries = store.list_memory(None, None, usize::MAX)?;
    if !entries.is_empty() {
        let embedder = Embedder::get()?;
        let texts: Vec<String> = entries.iter().map(|e| searchable_text(&e.record)).collect();
        let texts_ref: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let vecs = embedder.embed(&texts_ref)?;

        index
            .reserve(entries.len() + 64)
            .map_err(|e| anyhow::anyhow!("usearch reserve: {e}"))?;
        for (entry, vec) in entries.iter().zip(vecs) {
            index
                .add(entry.id as u64, &vec)
                .map_err(|e| anyhow::anyhow!("usearch add: {e}"))?;
        }
    }
    *hnsw = Some(index);
    Ok(())
}

/// Async facade over a shared [`Store`] used for memory reads and writes.
///
/// Cloning is cheap — `MemoryStore` is `Arc` under the hood.
#[derive(Clone)]
pub struct MemoryStore {
    store: Arc<Mutex<Store>>,
    hnsw_index: Arc<Mutex<Option<usearch::Index>>>,
}

impl MemoryStore {
    /// Wrap a shared [`Store`] for async memory access.
    ///
    /// Note: takes `Arc<Mutex<Store>>` rather than the spec-sketched
    /// `Arc<Store>` because `Store` is `!Sync` (rusqlite
    /// `Connection`). The async signature is preserved to match the
    /// documented API.
    pub async fn new(store: Arc<Mutex<Store>>) -> Self {
        let store_clone = Arc::clone(&store);
        let hnsw = tokio::task::spawn_blocking(move || -> Option<usearch::Index> {
            let options = usearch::IndexOptions {
                dimensions: 384,
                metric: usearch::MetricKind::Cos,
                quantization: usearch::ScalarKind::F32,
                ..Default::default()
            };
            let index = usearch::new_index(&options).ok()?;

            let guard = store_clone.lock().ok()?;
            let entries = guard.list_memory(None, None, usize::MAX).ok()?;

            if !entries.is_empty() {
                let embedder = Embedder::get().ok()?;
                let texts: Vec<String> =
                    entries.iter().map(|e| searchable_text(&e.record)).collect();
                let texts_ref: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
                let vecs = embedder.embed(&texts_ref).ok()?;

                index.reserve(entries.len() + 64).ok()?;
                for (entry, vec) in entries.iter().zip(vecs) {
                    index.add(entry.id as u64, &vec).ok()?;
                }
            }
            Some(index)
        })
        .await
        .unwrap_or(None);

        Self {
            store,
            hnsw_index: Arc::new(Mutex::new(hnsw)),
        }
    }

    /// Append a memory record. Called by workers after a subtask
    /// reaches `VerifyResult::Pass`.
    pub async fn record(&self, record: MemoryRecord) -> Result<()> {
        let store = Arc::clone(&self.store);
        let record_clone = record.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = store
                .lock()
                .map_err(|e| anyhow::anyhow!("store mutex poisoned: {e}"))?;
            guard.append_memory(&record_clone)?;
            Ok(())
        })
        .await
        .context("spawn_blocking join")??;

        // Add to HNSW index in the background/blocking task
        let store_clone = Arc::clone(&self.store);
        let hnsw_clone = Arc::clone(&self.hnsw_index);
        let record_for_embed = record.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut hnsw_guard = hnsw_clone
                .lock()
                .map_err(|e| anyhow::anyhow!("HNSW lock poisoned: {e}"))?;

            if let Some(hnsw) = hnsw_guard.as_mut() {
                let guard = store_clone
                    .lock()
                    .map_err(|e| anyhow::anyhow!("store mutex poisoned: {e}"))?;
                let entries = guard.list_memory(None, None, 1)?;
                if let Some(entry) = entries.first() {
                    let text = searchable_text(&record_for_embed);
                    let embedder = Embedder::get()?;
                    let vecs = embedder.embed(&[&text])?;
                    if let Some(vec) = vecs.into_iter().next() {
                        if hnsw.capacity() <= hnsw.size() {
                            hnsw.reserve(hnsw.size().saturating_add(64))
                                .map_err(|e| anyhow::anyhow!("usearch reserve: {e}"))?;
                        }
                        hnsw.add(entry.id as u64, &vec)
                            .map_err(|e| anyhow::anyhow!("usearch add: {e}"))?;
                    }
                }
            }
            Ok(())
        })
        .await
        .context("spawn_blocking join")??;

        Ok(())
    }

    /// Perform a high-speed HNSW semantic cosine retrieval pass.
    pub async fn query_semantic(&self, query: &str, limit: usize) -> Result<Vec<MemoryRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let store_clone = Arc::clone(&self.store);
        let hnsw_clone = Arc::clone(&self.hnsw_index);
        let query_str = query.to_string();

        let records = tokio::task::spawn_blocking(move || -> Result<Vec<MemoryRecord>> {
            let hnsw_guard = hnsw_clone
                .lock()
                .map_err(|e| anyhow::anyhow!("HNSW lock poisoned: {e}"))?;

            let hnsw = hnsw_guard
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("HNSW index not initialized"))?;

            let embedder = Embedder::get()?;
            let vecs = embedder.embed(&[&query_str])?;
            let query_vec = vecs
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("failed to generate embedding"))?;

            let matches = hnsw
                .search(&query_vec, limit)
                .map_err(|e| anyhow::anyhow!("usearch search failed: {e}"))?;

            if matches.keys.is_empty() {
                return Ok(Vec::new());
            }

            let store_guard = store_clone
                .lock()
                .map_err(|e| anyhow::anyhow!("store mutex poisoned: {e}"))?;

            let mut results = Vec::new();
            for (idx, &key) in matches.keys.iter().enumerate() {
                let dist = matches.distances[idx];
                if dist < 0.6 {
                    if let Some(entry) = store_guard.get_memory(key as i64)? {
                        results.push(entry.record);
                    }
                }
            }
            Ok(results)
        })
        .await
        .context("spawn_blocking join")??;

        Ok(records)
    }

    /// Return up to `limit` records ranked by relevance.
    /// Defaults to semantic HNSW retrieval, with keyword-overlap fallback.
    pub async fn query(&self, goal_text: &str, limit: usize) -> Result<Vec<MemoryRecord>> {
        match self.query_semantic(goal_text, limit).await {
            Ok(res) => Ok(res),
            Err(_) => {
                // Fallback to keyword-overlap
                if limit == 0 {
                    return Ok(Vec::new());
                }
                let goal_tokens = tokenize(goal_text);
                if goal_tokens.is_empty() {
                    return Ok(Vec::new());
                }

                let all = self.load_all().await?;
                let mut scored: Vec<(usize, MemoryRecord)> = all
                    .into_iter()
                    .map(|rec| {
                        let text = searchable_text(&rec);
                        let rec_tokens = tokenize(&text);
                        let score = goal_tokens.intersection(&rec_tokens).count();
                        (score, rec)
                    })
                    .filter(|(score, _)| *score > 0)
                    .collect();

                scored.sort_by_key(|entry| std::cmp::Reverse(entry.0));
                scored.truncate(limit);
                Ok(scored.into_iter().map(|(_, r)| r).collect())
            }
        }
    }

    /// Return every record whose stored `kind` column matches `kind`.
    /// `kind` is compared verbatim against the strings produced by
    /// [`phonton_store::MemoryKind::as_str`] (`"Decision"`,
    /// `"Constraint"`, `"RejectedApproach"`, `"Convention"`).
    pub async fn by_kind(&self, kind: &str) -> Result<Vec<MemoryRecord>> {
        let wanted = kind.to_string();
        let all = self.load_all().await?;
        Ok(all
            .into_iter()
            .filter(|r| record_kind_str(r) == wanted)
            .collect())
    }

    /// List memory records with editable ids and pin state.
    pub async fn list(
        &self,
        kind: Option<String>,
        topic: Option<String>,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        let store = Arc::clone(&self.store);
        let out = tokio::task::spawn_blocking(move || -> Result<Vec<MemoryEntry>> {
            let guard = store
                .lock()
                .map_err(|e| anyhow::anyhow!("store mutex poisoned: {e}"))?;
            guard.list_memory(kind.as_deref(), topic.as_deref(), limit)
        })
        .await
        .context("spawn_blocking join")??;
        Ok(out)
    }

    /// Fetch one memory entry by id.
    pub async fn get(&self, id: i64) -> Result<Option<MemoryEntry>> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || -> Result<Option<MemoryEntry>> {
            let guard = store
                .lock()
                .map_err(|e| anyhow::anyhow!("store mutex poisoned: {e}"))?;
            guard.get_memory(id)
        })
        .await
        .context("spawn_blocking join")?
    }

    /// Replace one memory record while preserving its id and pin state.
    pub async fn update(&self, id: i64, record: MemoryRecord) -> Result<bool> {
        let store = Arc::clone(&self.store);
        let record_clone = record.clone();
        let changed = tokio::task::spawn_blocking(move || -> Result<bool> {
            let guard = store
                .lock()
                .map_err(|e| anyhow::anyhow!("store mutex poisoned: {e}"))?;
            guard.update_memory(id, &record_clone)
        })
        .await
        .context("spawn_blocking join")??;

        if changed {
            let store_clone = Arc::clone(&self.store);
            let hnsw_clone = Arc::clone(&self.hnsw_index);
            tokio::task::spawn_blocking(move || -> Result<()> {
                let mut hnsw_guard = hnsw_clone
                    .lock()
                    .map_err(|e| anyhow::anyhow!("HNSW lock poisoned: {e}"))?;
                let guard = store_clone
                    .lock()
                    .map_err(|e| anyhow::anyhow!("store mutex poisoned: {e}"))?;
                rebuild_hnsw_index_sync(&guard, &mut hnsw_guard)?;
                Ok(())
            })
            .await
            .context("spawn_blocking join")??;
        }

        Ok(changed)
    }

    /// Delete one memory record.
    pub async fn delete(&self, id: i64) -> Result<bool> {
        let store = Arc::clone(&self.store);
        let changed = tokio::task::spawn_blocking(move || -> Result<bool> {
            let guard = store
                .lock()
                .map_err(|e| anyhow::anyhow!("store mutex poisoned: {e}"))?;
            guard.delete_memory(id)
        })
        .await
        .context("spawn_blocking join")??;

        if changed {
            let store_clone = Arc::clone(&self.store);
            let hnsw_clone = Arc::clone(&self.hnsw_index);
            tokio::task::spawn_blocking(move || -> Result<()> {
                let mut hnsw_guard = hnsw_clone
                    .lock()
                    .map_err(|e| anyhow::anyhow!("HNSW lock poisoned: {e}"))?;
                let guard = store_clone
                    .lock()
                    .map_err(|e| anyhow::anyhow!("store mutex poisoned: {e}"))?;
                rebuild_hnsw_index_sync(&guard, &mut hnsw_guard)?;
                Ok(())
            })
            .await
            .context("spawn_blocking join")??;
        }

        Ok(changed)
    }

    /// Pin or unpin a memory record.
    pub async fn set_pinned(&self, id: i64, pinned: bool) -> Result<bool> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || -> Result<bool> {
            let guard = store
                .lock()
                .map_err(|e| anyhow::anyhow!("store mutex poisoned: {e}"))?;
            guard.set_memory_pinned(id, pinned)
        })
        .await
        .context("spawn_blocking join")?
    }

    /// Pull every record out of the store.
    pub async fn load_all(&self) -> Result<Vec<MemoryRecord>> {
        let store = Arc::clone(&self.store);
        let out = tokio::task::spawn_blocking(move || -> Result<Vec<MemoryRecord>> {
            let guard = store
                .lock()
                .map_err(|e| anyhow::anyhow!("store mutex poisoned: {e}"))?;
            let rows = guard.search_memory("", None, usize::MAX)?;
            Ok(rows)
        })
        .await
        .context("spawn_blocking join")??;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Tokenise `text` on whitespace and ASCII punctuation, lowercase, dedup.
/// Empty tokens (possible after stripping punctuation runs) are dropped.
fn tokenize(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_lowercase())
        .collect()
}

/// Flatten a `MemoryRecord` to the text fields that should participate in
/// matching.
fn searchable_text(r: &MemoryRecord) -> String {
    match r {
        MemoryRecord::Decision { title, body, .. } => format!("{title} {body}"),
        MemoryRecord::Constraint {
            statement,
            rationale,
        } => format!("{statement} {rationale}"),
        MemoryRecord::RejectedApproach { summary, reason } => format!("{summary} {reason}"),
        MemoryRecord::Convention { rule, scope } => {
            format!("{rule} {}", scope.as_deref().unwrap_or(""))
        }
    }
}

fn record_kind_str(r: &MemoryRecord) -> &'static str {
    match r {
        MemoryRecord::Decision { .. } => "Decision",
        MemoryRecord::Constraint { .. } => "Constraint",
        MemoryRecord::RejectedApproach { .. } => "RejectedApproach",
        MemoryRecord::Convention { .. } => "Convention",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::MemoryRecord;

    async fn mem_store() -> MemoryStore {
        let store = Store::in_memory().expect("open in-memory store");
        MemoryStore::new(Arc::new(Mutex::new(store))).await
    }

    #[tokio::test]
    async fn query_ranks_by_keyword_overlap() {
        let ms = mem_store().await;

        ms.record(MemoryRecord::Decision {
            title: "use mpsc channels for worker fanout".into(),
            body: "switched away from shared RwLock".into(),
            task_id: None,
        })
        .await
        .unwrap();

        ms.record(MemoryRecord::RejectedApproach {
            summary: "global Arc RwLock context manager".into(),
            reason: "lock contention under parallel workers".into(),
        })
        .await
        .unwrap();

        ms.record(MemoryRecord::Convention {
            rule: "prefer thiserror over anyhow in libraries".into(),
            scope: Some("all crates except binaries".into()),
        })
        .await
        .unwrap();

        // Goal with heavy overlap on the RejectedApproach tokens.
        let hits = ms
            .query("RwLock context manager in parallel workers", 3)
            .await
            .unwrap();
        assert!(!hits.is_empty(), "expected at least one hit");
        match &hits[0] {
            MemoryRecord::RejectedApproach { summary, .. } => {
                assert!(
                    summary.contains("RwLock"),
                    "top hit should be the RwLock rejected approach, got: {summary}"
                );
            }
            other => panic!("top hit was not the expected RejectedApproach: {other:?}"),
        }
    }

    #[tokio::test]
    async fn query_respects_limit_and_zero_overlap_filter() {
        let ms = mem_store().await;
        ms.record(MemoryRecord::Decision {
            title: "database pool size configuration".into(),
            body: "".into(),
            task_id: None,
        })
        .await
        .unwrap();
        ms.record(MemoryRecord::Decision {
            title: "encryption protocols for network socket".into(),
            body: "".into(),
            task_id: None,
        })
        .await
        .unwrap();

        // completely unrelated, overlaps nothing, high distance
        let none = ms
            .query("compile compiler warnings and release packaging", 5)
            .await
            .unwrap();
        assert!(none.is_empty());

        // limit=1 returns a single ranked record.
        let capped = ms.query("database encryption", 1).await.unwrap();
        assert_eq!(capped.len(), 1);
    }

    #[tokio::test]
    async fn by_kind_filters() {
        let ms = mem_store().await;
        ms.record(MemoryRecord::Decision {
            title: "d1".into(),
            body: "".into(),
            task_id: None,
        })
        .await
        .unwrap();
        ms.record(MemoryRecord::Constraint {
            statement: "phonton-types stays tokio-free".into(),
            rationale: "keep the type crate cheap to depend on".into(),
        })
        .await
        .unwrap();

        let decisions = ms.by_kind("Decision").await.unwrap();
        assert_eq!(decisions.len(), 1);
        let constraints = ms.by_kind("Constraint").await.unwrap();
        assert_eq!(constraints.len(), 1);
        let none = ms.by_kind("Convention").await.unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn lifecycle_list_edit_pin_delete() {
        let ms = mem_store().await;
        ms.record(MemoryRecord::Convention {
            rule: "prefer small modules".into(),
            scope: Some("planner".into()),
        })
        .await
        .unwrap();

        let entries = ms
            .list(Some("Convention".into()), Some("planner".into()), 10)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        let id = entries[0].id;

        assert!(ms.set_pinned(id, true).await.unwrap());
        assert!(ms
            .update(
                id,
                MemoryRecord::Convention {
                    rule: "prefer focused modules".into(),
                    scope: Some("planner".into()),
                },
            )
            .await
            .unwrap());

        let updated = ms.get(id).await.unwrap().unwrap();
        assert!(updated.pinned);
        match updated.record {
            MemoryRecord::Convention { rule, .. } => {
                assert_eq!(rule, "prefer focused modules");
            }
            other => panic!("unexpected record: {other:?}"),
        }

        assert!(ms.delete(id).await.unwrap());
        assert!(ms.get(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn query_semantic_matches_synonyms() {
        let ms = mem_store().await;

        ms.record(MemoryRecord::Decision {
            title: "Avoid panics during network session initialization".into(),
            body: "Always propagate errors back to caller".into(),
            task_id: None,
        })
        .await
        .unwrap();

        ms.record(MemoryRecord::Constraint {
            statement: "Check authentication credentials on boot".into(),
            rationale: "Verify keys before launching the service".into(),
        })
        .await
        .unwrap();

        // Query for semantic synonym with zero keyword overlap:
        let hits = ms
            .query_semantic("do not crash when starting a connection", 1)
            .await
            .unwrap();
        assert!(!hits.is_empty(), "expected semantic hit");
        match &hits[0] {
            MemoryRecord::Decision { title, .. } => {
                assert!(
                    title.contains("Avoid panics"),
                    "should match network session initialization, got: {}",
                    title
                );
            }
            other => panic!("expected Decision hit, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn benchmark_latency_concurrent() {
        let store = Store::in_memory().expect("open in-memory store");

        // Populate 1,000 memories in the database directly first to allow batch embedding
        for i in 0..1000 {
            let rec = MemoryRecord::Decision {
                title: format!("Decision number {}", i),
                body: format!("This is the body text for decision number {}. We want some content here to make it realistic.", i),
                task_id: None,
            };
            store.append_memory(&rec).expect("append memory");
        }

        // Build the MemoryStore. It will batch-embed all 1,000 records on initialization.
        let store_arc = Arc::new(Mutex::new(store));
        let ms = MemoryStore::new(store_arc).await;

        // Pre-generate 100 query embeddings to measure HNSW retrieval latency itself,
        // rather than the CPU ONNX embedder runtime latency in unoptimized debug test environments.
        let embedder = Embedder::get().expect("get embedder");
        let mut query_vecs = Vec::new();
        for i in 0..100 {
            let query = format!("Querying about decision number {}", i * 10 % 1000);
            let vecs = embedder.embed(&[&query]).expect("embed");
            query_vecs.push(vecs.into_iter().next().expect("vec"));
        }

        let hnsw_index = Arc::clone(&ms.hnsw_index);
        let start = std::time::Instant::now();
        let mut tasks = Vec::new();
        for query_vec in query_vecs {
            let hnsw_clone = Arc::clone(&hnsw_index);
            tasks.push(tokio::spawn(async move {
                let hnsw_guard = hnsw_clone
                    .lock()
                    .map_err(|e| anyhow::anyhow!("HNSW lock poisoned: {e}"))?;
                let hnsw = hnsw_guard
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("HNSW index not initialized"))?;
                let matches = hnsw
                    .search(&query_vec, 5)
                    .map_err(|e| anyhow::anyhow!("usearch search failed: {e}"))?;
                Ok::<_, anyhow::Error>(matches)
            }));
        }

        // Run them concurrently and wait for all to complete
        let mut success_count = 0;
        for t in tasks {
            let res = t.await.unwrap();
            if res.is_ok() {
                success_count += 1;
            }
        }

        let elapsed = start.elapsed();
        let avg_latency = elapsed / 100;
        println!(
            "100 concurrent queries over 1,000 records completed in {:?}. Average latency: {:?}",
            elapsed, avg_latency
        );
        assert_eq!(success_count, 100);
        assert!(
            avg_latency < std::time::Duration::from_millis(10),
            "Average latency must be under 10ms"
        );
    }
}
