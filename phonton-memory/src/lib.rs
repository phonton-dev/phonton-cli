//! Cross-session memory facade over [`phonton_store::Store`].
//!
//! Workers call [`MemoryStore::record`] after a subtask completes to log
//! a `Decision`, `Constraint`, `RejectedApproach`, or `Convention`. The
//! planner calls [`MemoryStore::query`] before decomposing a new goal to
//! surface prior context relevant to the request.
//!
//! Relevance at this layer is local and deterministic: tokenisation,
//! light stemming, a small synonym table for engineering terms, stopword
//! filtering, and IDF-weighted overlap. It is not a replacement for
//! embeddings, but it avoids the most brittle exact-keyword misses.
//!
//! ## Storage note
//!
//! The spec sketches `Arc<Store>`, but `rusqlite::Connection` is `!Sync`,
//! so sharing a single `Store` across tokio tasks requires a mutex. This
//! crate follows the established phonton-worker pattern and wraps the
//! store in `Arc<Mutex<Store>>` internally. `MemoryStore` exposes an
//! async API so planner/worker call sites don't have to reach around it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use phonton_store::{MemoryEntry, Store};
use phonton_types::MemoryRecord;

/// Async facade over a shared [`Store`] used for memory reads and writes.
///
/// Cloning is cheap — `MemoryStore` is `Arc` under the hood.
#[derive(Clone)]
pub struct MemoryStore {
    store: Arc<Mutex<Store>>,
}

impl MemoryStore {
    /// Wrap a shared [`Store`] for async memory access.
    ///
    /// Note: takes `Arc<Mutex<Store>>` rather than the spec-sketched
    /// `Arc<Store>` because `Store` is `!Sync` (rusqlite
    /// `Connection`). The async signature is preserved to match the
    /// documented API.
    pub async fn new(store: Arc<Mutex<Store>>) -> Self {
        Self { store }
    }

    /// Append a memory record. Called by workers after a subtask
    /// reaches `VerifyResult::Pass`.
    pub async fn record(&self, record: MemoryRecord) -> Result<()> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = store
                .lock()
                .map_err(|e| anyhow::anyhow!("store mutex poisoned: {e}"))?;
            guard.append_memory(&record)?;
            Ok(())
        })
        .await
        .context("spawn_blocking join")??;
        Ok(())
    }

    /// Return up to `limit` records ranked by weighted local relevance with
    /// `goal_text`. Records with zero shared terms are excluded.
    pub async fn query(&self, goal_text: &str, limit: usize) -> Result<Vec<MemoryRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let goal_tokens = tokenize(goal_text);
        if goal_tokens.is_empty() {
            return Ok(Vec::new());
        }

        let all = self.load_all().await?;
        let mut docs: Vec<(HashSet<String>, MemoryRecord)> = all
            .into_iter()
            .map(|rec| (tokenize(&searchable_text(&rec)), rec))
            .collect();
        let doc_freq = document_frequencies(docs.iter().map(|(tokens, _)| tokens));
        let doc_count = docs.len() as f64;
        let mut scored: Vec<(f64, MemoryRecord)> = docs
            .drain(..)
            .filter_map(|(rec_tokens, rec)| {
                let score = weighted_overlap_score(&goal_tokens, &rec_tokens, &doc_freq, doc_count);
                (score > 0.0).then_some((score, rec))
            })
            .collect();

        // Descending by score. Stable sort keeps insertion order as the
        // tiebreaker for equally relevant entries.
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        scored.truncate(limit);
        Ok(scored.into_iter().map(|(_, r)| r).collect())
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
        tokio::task::spawn_blocking(move || -> Result<bool> {
            let guard = store
                .lock()
                .map_err(|e| anyhow::anyhow!("store mutex poisoned: {e}"))?;
            guard.update_memory(id, &record)
        })
        .await
        .context("spawn_blocking join")?
    }

    /// Delete one memory record.
    pub async fn delete(&self, id: i64) -> Result<bool> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || -> Result<bool> {
            let guard = store
                .lock()
                .map_err(|e| anyhow::anyhow!("store mutex poisoned: {e}"))?;
            guard.delete_memory(id)
        })
        .await
        .context("spawn_blocking join")?
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

    /// Pull every record out of the store. The phonton-store public API
    /// only exposes topic-filtered lookups; we use the widest possible
    /// substring (`""`, which `LIKE '%%'` matches for every row) and let
    /// `usize::MAX` stand in for "no limit".
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

/// Tokenise `text` on whitespace and punctuation, lowercase, lightly stem,
/// expand selected engineering synonyms, and dedup.
fn tokenize(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        let lower = raw.to_ascii_lowercase();
        let stemmed = stem(&lower);
        if stemmed.is_empty() || is_stopword(&stemmed) {
            continue;
        }
        out.insert(stemmed.clone());
        for related in related_terms(&stemmed) {
            out.insert(related.to_string());
        }
    }
    out
}

fn document_frequencies<'a>(
    docs: impl Iterator<Item = &'a HashSet<String>>,
) -> HashMap<String, usize> {
    let mut freqs = HashMap::new();
    for doc in docs {
        for term in doc {
            *freqs.entry(term.clone()).or_insert(0) += 1;
        }
    }
    freqs
}

fn weighted_overlap_score(
    query: &HashSet<String>,
    record: &HashSet<String>,
    doc_freq: &HashMap<String, usize>,
    doc_count: f64,
) -> f64 {
    query
        .intersection(record)
        .map(|term| {
            let df = *doc_freq.get(term.as_str()).unwrap_or(&1) as f64;
            ((doc_count + 1.0) / (df + 1.0)).ln() + 1.0
        })
        .sum()
}

fn stem(token: &str) -> String {
    let mut s = token.to_string();
    if s.len() > 5 && s.ends_with("ing") {
        s.truncate(s.len() - 3);
    } else if s.len() > 4 && s.ends_with("ied") {
        s.truncate(s.len() - 3);
        s.push('y');
    } else if s.len() > 4 && (s.ends_with("ed") || s.ends_with("es")) {
        s.truncate(s.len() - 2);
    } else if s.len() > 3 && s.ends_with('s') {
        s.truncate(s.len() - 1);
    }

    if s.ends_with("nn") || s.ends_with("ll") || s.ends_with("tt") || s.ends_with("pp") {
        s.pop();
    }
    if s.ends_with("ick") {
        s.pop();
    }
    s
}

fn related_terms(term: &str) -> &'static [&'static str] {
    match term {
        "synchronou" | "sync" | "stall" | "wait" | "block" => {
            &["block", "synchronou", "stall", "wait"]
        }
        "panic" | "crash" | "abort" => &["panic", "crash", "abort"],
        "thread" | "task" | "worker" => &["concurrency"],
        "async" | "concurrency" => &["concurrency"],
        _ => &[],
    }
}

fn is_stopword(term: &str) -> bool {
    matches!(
        term,
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "by"
            | "for"
            | "from"
            | "has"
            | "have"
            | "in"
            | "into"
            | "is"
            | "it"
            | "of"
            | "on"
            | "or"
            | "that"
            | "the"
            | "this"
            | "to"
            | "use"
            | "with"
            | "without"
            | "code"
            | "file"
            | "implementation"
            | "task"
            | "worker"
    )
}

/// Flatten a `MemoryRecord` to the text fields that should participate in
/// keyword matching. Mirrors the `topic` denormalisation in
/// `phonton-store` so scoring stays consistent with what's indexed there.
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

    fn mem_store() -> MemoryStore {
        let store = Store::in_memory().expect("open in-memory store");
        MemoryStore {
            store: Arc::new(Mutex::new(store)),
        }
    }

    #[tokio::test]
    async fn query_ranks_by_keyword_overlap() {
        let ms = mem_store();

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
        let ms = mem_store();
        ms.record(MemoryRecord::Decision {
            title: "alpha beta gamma".into(),
            body: "".into(),
            task_id: None,
        })
        .await
        .unwrap();
        ms.record(MemoryRecord::Decision {
            title: "delta epsilon".into(),
            body: "".into(),
            task_id: None,
        })
        .await
        .unwrap();

        // "zeta" overlaps nothing.
        let none = ms.query("zeta", 5).await.unwrap();
        assert!(none.is_empty());

        // limit=1 returns a single ranked record.
        let capped = ms.query("alpha delta", 1).await.unwrap();
        assert_eq!(capped.len(), 1);
    }

    #[tokio::test]
    async fn query_matches_stemmed_and_related_terms() {
        let ms = mem_store();
        ms.record(MemoryRecord::Constraint {
            statement: "Avoid thread blocking calls".into(),
            rationale: "prevents hidden concurrency delays".into(),
        })
        .await
        .unwrap();

        let hits = ms
            .query("synchronous stalls in async tasks", 5)
            .await
            .unwrap();

        assert!(
            matches!(hits.first(), Some(MemoryRecord::Constraint { statement, .. }) if statement.contains("blocking")),
            "expected blocking-call constraint from stemmed/synonym query, got {hits:?}"
        );
    }

    #[tokio::test]
    async fn query_downweights_common_words_against_specific_memory() {
        let ms = mem_store();
        ms.record(MemoryRecord::Convention {
            rule: "Use the code in the worker task file for the task".into(),
            scope: Some("general implementation wording".into()),
        })
        .await
        .unwrap();
        ms.record(MemoryRecord::Constraint {
            statement: "Avoid thread blocking calls".into(),
            rationale: "prevents hidden concurrency delays".into(),
        })
        .await
        .unwrap();

        let hits = ms
            .query("the worker task has synchronous stalls in code", 2)
            .await
            .unwrap();

        assert!(
            matches!(hits.first(), Some(MemoryRecord::Constraint { .. })),
            "specific concurrency record should outrank common wording, got {hits:?}"
        );
    }

    #[tokio::test]
    async fn by_kind_filters() {
        let ms = mem_store();
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
        let ms = mem_store();
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
}
