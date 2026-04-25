//! Cross-session memory facade over [`phonton_store::Store`].
//!
//! Workers call [`MemoryStore::record`] after a subtask completes to log
//! a `Decision`, `Constraint`, `RejectedApproach`, or `Convention`. The
//! planner calls [`MemoryStore::query`] before decomposing a new goal to
//! surface prior context relevant to the request.
//!
//! Relevance at this layer is **keyword-overlap only** — no embeddings,
//! no FTS. Each record's searchable text is tokenised (whitespace +
//! ASCII punctuation, case-insensitive), deduplicated, and scored by the
//! size of its intersection with the goal's token set. Good enough to
//! surface obvious hits; explicitly not good enough for semantic recall.
//!
//! ## Storage note
//!
//! The spec sketches `Arc<Store>`, but `rusqlite::Connection` is `!Sync`,
//! so sharing a single `Store` across tokio tasks requires a mutex. This
//! crate follows the established phonton-worker pattern and wraps the
//! store in `Arc<Mutex<Store>>` internally. `MemoryStore` exposes an
//! async API so planner/worker call sites don't have to reach around it.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use phonton_store::Store;
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
            let guard = store.lock().map_err(|e| anyhow::anyhow!("store mutex poisoned: {e}"))?;
            guard.append_memory(&record)?;
            Ok(())
        })
        .await
        .context("spawn_blocking join")??;
        Ok(())
    }

    /// Return up to `limit` records ranked by keyword overlap with
    /// `goal_text`. Records with zero shared tokens are excluded.
    pub async fn query(&self, goal_text: &str, limit: usize) -> Result<Vec<MemoryRecord>> {
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

        // Descending by score. Stable sort keeps insertion order (which
        // reflects recency, since we selected without an ORDER BY) as the
        // tiebreaker — slightly preferring older entries. Good enough.
        scored.sort_by(|a, b| b.0.cmp(&a.0));
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

    /// Pull every record out of the store. The phonton-store public API
    /// only exposes topic-filtered lookups; we use the widest possible
    /// substring (`""`, which `LIKE '%%'` matches for every row) and let
    /// `usize::MAX` stand in for "no limit".
    async fn load_all(&self) -> Result<Vec<MemoryRecord>> {
        let store = Arc::clone(&self.store);
        let out = tokio::task::spawn_blocking(move || -> Result<Vec<MemoryRecord>> {
            let guard = store.lock().map_err(|e| anyhow::anyhow!("store mutex poisoned: {e}"))?;
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
/// keyword matching. Mirrors the `topic` denormalisation in
/// `phonton-store` so scoring stays consistent with what's indexed there.
fn searchable_text(r: &MemoryRecord) -> String {
    match r {
        MemoryRecord::Decision { title, body, .. } => format!("{title} {body}"),
        MemoryRecord::Constraint { statement, rationale } => format!("{statement} {rationale}"),
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
}
