//! SQLite persistence: task history, memory records, and the warm-crate
//! cache that lets `phonton-verify` skip Layer 2 when a crate was checked
//! recently and no files have changed.
//!
//! All schema lives in [`MIGRATIONS`] and is applied idempotently in
//! [`Store::open`]. The store is sync (`rusqlite`); call sites that live
//! on a Tokio runtime should wrap calls in `spawn_blocking`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use phonton_types::{EventRecord, MemoryRecord, TaskId, TaskStatus};
use rusqlite::{params, Connection, OptionalExtension};

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Schema applied at [`Store::open`] time. Each statement is idempotent
/// (`IF NOT EXISTS`) so re-opening an existing DB is a no-op.
const MIGRATIONS: &str = "
CREATE TABLE IF NOT EXISTS tasks (
    id            TEXT PRIMARY KEY,
    goal_text     TEXT NOT NULL,
    status_json   TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    total_tokens  INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS memory_records (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL,        -- 'Decision' | 'Constraint' | 'RejectedApproach' | 'Convention'
    body_json   TEXT NOT NULL,        -- full MemoryRecord serialised
    topic       TEXT NOT NULL,        -- denormalised for FTS-lite LIKE matching
    task_id     TEXT,                 -- nullable; only set for Decisions tied to a task
    created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_memory_kind  ON memory_records(kind);
CREATE INDEX IF NOT EXISTS idx_memory_topic ON memory_records(topic);

CREATE TABLE IF NOT EXISTS orchestrator_events (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id       TEXT NOT NULL,
    kind          TEXT NOT NULL,         -- denormalised event-kind tag
    timestamp_ms  INTEGER NOT NULL,
    body_json     TEXT NOT NULL          -- full EventRecord serialised
);
CREATE INDEX IF NOT EXISTS idx_events_task_ts
    ON orchestrator_events(task_id, timestamp_ms);

CREATE TABLE IF NOT EXISTS warm_crates (
    crate_name        TEXT PRIMARY KEY,
    last_checked_at   INTEGER NOT NULL,  -- unix seconds
    last_files_hash   TEXT NOT NULL      -- caller-supplied hash of crate sources
);
";

/// How long a `warm_crates` row stays valid. See
/// `01-architecture/failure-modes.md` Risk 1.
pub const WARM_TTL_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Handle to the SQLite-backed phonton store.
///
/// Cheap to construct repeatedly (it just wraps a [`Connection`]); cheap
/// to share across a single thread. For multi-threaded access open one
/// `Store` per thread or wrap with `Mutex`.
pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    /// Open (or create) the store at `path`, applying [`MIGRATIONS`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open(&path)
            .with_context(|| format!("opening sqlite db at {}", path.display()))?;
        conn.execute_batch(MIGRATIONS)
            .context("applying phonton-store migrations")?;
        Ok(Self { conn, path })
    }

    /// Open an in-memory store. Useful for tests and ephemeral runs.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory sqlite")?;
        conn.execute_batch(MIGRATIONS)
            .context("applying phonton-store migrations")?;
        Ok(Self {
            conn,
            path: PathBuf::from(":memory:"),
        })
    }

    /// Filesystem path the store was opened with (or `:memory:`).
    pub fn path(&self) -> &Path {
        &self.path
    }

    // -----------------------------------------------------------------
    // Tasks
    // -----------------------------------------------------------------

    /// Insert or replace a task record.
    pub fn upsert_task(
        &self,
        id: TaskId,
        goal_text: &str,
        status: &TaskStatus,
        total_tokens: u64,
    ) -> Result<()> {
        let status_json = serde_json::to_string(status)?;
        self.conn.execute(
            "INSERT INTO tasks (id, goal_text, status_json, created_at, total_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                 goal_text    = excluded.goal_text,
                 status_json  = excluded.status_json,
                 total_tokens = excluded.total_tokens",
            params![id.to_string(), goal_text, status_json, now_secs() as i64, total_tokens as i64],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Memory
    // -----------------------------------------------------------------

    /// Append a [`MemoryRecord`]. Records are immutable; updates require
    /// a new insert.
    pub fn append_memory(&self, record: &MemoryRecord) -> Result<()> {
        let kind = memory_kind(record);
        let topic = memory_topic(record);
        let task_id = memory_task_id(record).map(|t| t.to_string());
        let body = serde_json::to_string(record)?;
        self.conn.execute(
            "INSERT INTO memory_records (kind, body_json, topic, task_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kind, body, topic, task_id, now_secs() as i64],
        )?;
        Ok(())
    }

    /// Free-form memory query, ranked by recency. `kind_filter` narrows
    /// to a single variant; `description` is matched as a case-insensitive
    /// substring against the denormalised `topic` column.
    ///
    /// Used by the planner to fetch relevant prior decisions and — most
    /// importantly — `RejectedApproach` records before decomposing a new
    /// goal, so the same dead-end isn't re-proposed.
    pub fn search_memory(
        &self,
        description: &str,
        kind_filter: Option<MemoryKind>,
        top_k: usize,
    ) -> Result<Vec<MemoryRecord>> {
        let like = format!("%{}%", description.to_lowercase());
        let mut sql = String::from(
            "SELECT body_json FROM memory_records
             WHERE LOWER(topic) LIKE ?1",
        );
        if kind_filter.is_some() {
            sql.push_str(" AND kind = ?2");
        }
        sql.push_str(" ORDER BY created_at DESC LIMIT ?");
        // The LIMIT placeholder index depends on whether kind is bound.
        sql.push_str(if kind_filter.is_some() { "3" } else { "2" });

        let mut stmt = self.conn.prepare(&sql)?;
        let body_iter = if let Some(kind) = kind_filter {
            stmt.query_map(
                params![like, kind.as_str(), top_k as i64],
                |r| r.get::<_, String>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![like, top_k as i64], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };

        let mut out = Vec::with_capacity(body_iter.len());
        for body in body_iter {
            out.push(serde_json::from_str(&body)?);
        }
        Ok(out)
    }

    /// Convenience wrapper used by the planner: only `RejectedApproach`
    /// records, ranked by recency, top `n`.
    pub fn query_rejected_approaches(&self, topic: &str, n: usize) -> Result<Vec<MemoryRecord>> {
        self.search_memory(topic, Some(MemoryKind::RejectedApproach), n)
    }

    // -----------------------------------------------------------------
    // Orchestrator events — structured telemetry for the Flight Log
    // -----------------------------------------------------------------

    /// Append one [`EventRecord`]. Records are immutable.
    pub fn append_event(&self, record: &EventRecord) -> Result<()> {
        let body = serde_json::to_string(record)?;
        self.conn.execute(
            "INSERT INTO orchestrator_events (task_id, kind, timestamp_ms, body_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                record.task_id.to_string(),
                record.kind(),
                record.timestamp_ms as i64,
                body,
            ],
        )?;
        Ok(())
    }

    /// Fetch events for one task in chronological order, newest last.
    /// `limit` caps the number returned to keep the Flight Log bounded.
    pub fn list_events(&self, task_id: TaskId, limit: usize) -> Result<Vec<EventRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT body_json FROM orchestrator_events
             WHERE task_id = ?1
             ORDER BY timestamp_ms ASC, id ASC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![task_id.to_string(), limit as i64], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|s| serde_json::from_str::<EventRecord>(&s).map_err(Into::into))
            .collect()
    }

    // -----------------------------------------------------------------
    // Warm-crate cache (Risk 1 mitigation)
    // -----------------------------------------------------------------

    /// Mark `crate_name` as freshly checked. `files_hash` should be a
    /// stable digest of the crate's source tree so a later
    /// [`Store::is_crate_warm`] call can detect post-check edits.
    pub fn mark_crate_warm_sync(&self, crate_name: &str, files_hash: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO warm_crates (crate_name, last_checked_at, last_files_hash)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(crate_name) DO UPDATE SET
                 last_checked_at = excluded.last_checked_at,
                 last_files_hash = excluded.last_files_hash",
            params![crate_name, now_secs() as i64, files_hash],
        )?;
        Ok(())
    }

    /// Return `true` if `crate_name` was successfully checked within the
    /// last [`WARM_TTL_SECS`] **and** the supplied `files_hash` matches
    /// the one recorded at the previous check (i.e. nothing changed in
    /// between). `phonton-verify` consults this before invoking Layer 2.
    pub fn is_crate_warm_sync(&self, crate_name: &str, files_hash: &str) -> Result<bool> {
        let row: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT last_checked_at, last_files_hash FROM warm_crates
                 WHERE crate_name = ?1",
                params![crate_name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        let Some((last_at, last_hash)) = row else {
            return Ok(false);
        };
        if last_hash != files_hash {
            return Ok(false);
        }
        let age = now_secs().saturating_sub(last_at as u64);
        Ok(age < WARM_TTL_SECS)
    }

    /// Drop a single crate's warm entry — call from file-watcher code
    /// when a source file under that crate changes.
    pub fn invalidate_warm_crate(&self, crate_name: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM warm_crates WHERE crate_name = ?1",
            params![crate_name],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Async query API
    //
    // Thin wrappers over the sync methods above. rusqlite is blocking;
    // the `async fn` signature just lets these compose in async call
    // sites without threading a spawn_blocking around each call.
    // -----------------------------------------------------------------

    /// Fetch one task by id. `None` if no row matches.
    pub async fn get_task(&self, id: TaskId) -> Result<Option<TaskRecord>> {
        self.conn
            .query_row(
                "SELECT id, goal_text, status_json, created_at, total_tokens
                 FROM tasks WHERE id = ?1",
                params![id.to_string()],
                row_to_task,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Most recent `limit` tasks, newest first.
    pub async fn list_tasks(&self, limit: usize) -> Result<Vec<TaskRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, goal_text, status_json, created_at, total_tokens
             FROM tasks ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], row_to_task)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Overwrite the `status_json` column for one task. No-op if the id
    /// doesn't exist — callers must have upserted the task first.
    pub async fn update_task_status(
        &self,
        id: TaskId,
        status: serde_json::Value,
    ) -> Result<()> {
        let status_json = serde_json::to_string(&status)?;
        self.conn.execute(
            "UPDATE tasks SET status_json = ?1 WHERE id = ?2",
            params![status_json, id.to_string()],
        )?;
        Ok(())
    }

    /// Async counterpart to [`Store::append_memory`].
    pub async fn write_memory(&self, record: &MemoryRecord) -> Result<()> {
        self.append_memory(record)
    }

    /// Filter memory by `kind` and/or `topic` substring. `None` on either
    /// side removes that predicate. Ordered by recency.
    pub async fn query_memory(
        &self,
        kind: Option<&str>,
        topic: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>> {
        let mut sql =
            String::from("SELECT body_json FROM memory_records WHERE 1=1");
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(k) = kind {
            sql.push_str(" AND kind = ?");
            binds.push(Box::new(k.to_string()));
        }
        if let Some(t) = topic {
            sql.push_str(" AND LOWER(topic) LIKE ?");
            binds.push(Box::new(format!("%{}%", t.to_lowercase())));
        }
        sql.push_str(" ORDER BY created_at DESC LIMIT ?");
        binds.push(Box::new(limit as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let params_ref: Vec<&dyn rusqlite::ToSql> =
            binds.iter().map(|b| b.as_ref()).collect();
        let rows = stmt
            .query_map(params_ref.as_slice(), |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|s| serde_json::from_str::<MemoryRecord>(&s).map_err(Into::into))
            .collect()
    }

    /// Async counterpart to [`Store::is_crate_warm_sync`]. `ttl_secs` is
    /// fixed at [`WARM_TTL_SECS`].
    pub async fn is_crate_warm(&self, crate_name: &str, file_hash: &str) -> Result<bool> {
        self.is_crate_warm_sync(crate_name, file_hash)
    }

    /// Async counterpart to [`Store::mark_crate_warm_sync`].
    pub async fn mark_crate_warm(&self, crate_name: &str, file_hash: &str) -> Result<()> {
        self.mark_crate_warm_sync(crate_name, file_hash)
    }

    /// Delete warm-crate rows whose `last_checked_at` is older than
    /// `now - ttl_secs`. Returns the number of rows removed.
    pub async fn evict_stale_warm_crates(&self, ttl_secs: u64) -> Result<usize> {
        let cutoff = now_secs().saturating_sub(ttl_secs) as i64;
        let n = self.conn.execute(
            "DELETE FROM warm_crates WHERE last_checked_at < ?1",
            params![cutoff],
        )?;
        Ok(n)
    }
}

/// Denormalised task row, returned by [`Store::get_task`] and
/// [`Store::list_tasks`].
#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub id: TaskId,
    pub goal_text: String,
    pub status: serde_json::Value,
    pub created_at: u64,
    pub total_tokens: u64,
}

fn row_to_task(r: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    let id_str: String = r.get(0)?;
    let uuid = uuid::Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })?;
    let id = TaskId(uuid);
    let status_json: String = r.get(2)?;
    let status: serde_json::Value = serde_json::from_str(&status_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })?;
    let created_at: i64 = r.get(3)?;
    let total_tokens: i64 = r.get(4)?;
    Ok(TaskRecord {
        id,
        goal_text: r.get(1)?,
        status,
        created_at: created_at as u64,
        total_tokens: total_tokens as u64,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// String-typed [`MemoryRecord`] discriminator used in the `kind` column.
/// Keeps the enum-name-as-string discipline out of caller code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    Decision,
    Constraint,
    RejectedApproach,
    Convention,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Decision => "Decision",
            MemoryKind::Constraint => "Constraint",
            MemoryKind::RejectedApproach => "RejectedApproach",
            MemoryKind::Convention => "Convention",
        }
    }
}

fn memory_kind(r: &MemoryRecord) -> &'static str {
    match r {
        MemoryRecord::Decision { .. } => "Decision",
        MemoryRecord::Constraint { .. } => "Constraint",
        MemoryRecord::RejectedApproach { .. } => "RejectedApproach",
        MemoryRecord::Convention { .. } => "Convention",
    }
}

fn memory_topic(r: &MemoryRecord) -> String {
    match r {
        MemoryRecord::Decision { title, body, .. } => format!("{title} {body}"),
        MemoryRecord::Constraint { statement, rationale } => format!("{statement} {rationale}"),
        MemoryRecord::RejectedApproach { summary, reason } => format!("{summary} {reason}"),
        MemoryRecord::Convention { rule, scope } => {
            format!("{rule} {}", scope.as_deref().unwrap_or(""))
        }
    }
}

fn memory_task_id(r: &MemoryRecord) -> Option<TaskId> {
    match r {
        MemoryRecord::Decision { task_id, .. } => *task_id,
        _ => None,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::MemoryRecord;

    #[test]
    fn open_creates_schema() {
        let s = Store::in_memory().unwrap();
        // Querying a fresh DB should return an empty vec, not an error.
        let r = s.search_memory("anything", None, 10).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn append_and_query_rejected_approach() {
        let s = Store::in_memory().unwrap();
        s.append_memory(&MemoryRecord::RejectedApproach {
            summary: "use Arc<RwLock<_>> in context manager".into(),
            reason: "lock contention under parallel workers".into(),
        })
        .unwrap();
        s.append_memory(&MemoryRecord::Decision {
            title: "use mpsc instead".into(),
            body: "switched to channels".into(),
            task_id: None,
        })
        .unwrap();

        let rejected = s.query_rejected_approaches("Arc<RwLock", 5).unwrap();
        assert_eq!(rejected.len(), 1);
        match &rejected[0] {
            MemoryRecord::RejectedApproach { summary, .. } => {
                assert!(summary.contains("Arc<RwLock"));
            }
            other => panic!("unexpected kind: {other:?}"),
        }

        // Decision should not show up under the RejectedApproach filter.
        let none_rejected = s.query_rejected_approaches("mpsc", 5).unwrap();
        assert!(none_rejected.is_empty());
    }

    #[test]
    fn append_and_list_events_preserves_order_per_task() {
        use phonton_types::{OrchestratorEvent, SubtaskId};
        let s = Store::in_memory().unwrap();
        let t1 = TaskId::new();
        let t2 = TaskId::new();
        let sid = SubtaskId::new();
        let e1 = EventRecord {
            task_id: t1,
            timestamp_ms: 10,
            event: OrchestratorEvent::TaskStarted {
                task_id: t1,
                goal: "g".into(),
                subtask_count: 2,
            },
        };
        let e2 = EventRecord {
            task_id: t1,
            timestamp_ms: 20,
            event: OrchestratorEvent::SubtaskCompleted {
                subtask_id: sid,
                tokens_used: 42,
            },
        };
        let e_other = EventRecord {
            task_id: t2,
            timestamp_ms: 15,
            event: OrchestratorEvent::TaskStarted {
                task_id: t2,
                goal: "other".into(),
                subtask_count: 1,
            },
        };
        s.append_event(&e1).unwrap();
        s.append_event(&e_other).unwrap();
        s.append_event(&e2).unwrap();

        let for_t1 = s.list_events(t1, 100).unwrap();
        assert_eq!(for_t1.len(), 2);
        assert_eq!(for_t1[0].timestamp_ms, 10);
        assert_eq!(for_t1[1].timestamp_ms, 20);

        let for_t2 = s.list_events(t2, 100).unwrap();
        assert_eq!(for_t2.len(), 1);
    }

    #[test]
    fn warm_crate_round_trip() {
        let s = Store::in_memory().unwrap();
        assert!(!s.is_crate_warm_sync("phonton-types", "h1").unwrap());

        s.mark_crate_warm_sync("phonton-types", "h1").unwrap();
        assert!(s.is_crate_warm_sync("phonton-types", "h1").unwrap());

        // Different hash → cache miss even though within TTL.
        assert!(!s.is_crate_warm_sync("phonton-types", "h2").unwrap());

        s.invalidate_warm_crate("phonton-types").unwrap();
        assert!(!s.is_crate_warm_sync("phonton-types", "h1").unwrap());
    }

    // -----------------------------------------------------------------
    // Async query API
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn get_and_list_tasks() {
        let s = Store::in_memory().unwrap();
        let id = TaskId::new();
        s.upsert_task(id, "find the bug", &TaskStatus::Queued, 42).unwrap();

        let fetched = s.get_task(id).await.unwrap().expect("task exists");
        assert_eq!(fetched.goal_text, "find the bug");
        assert_eq!(fetched.total_tokens, 42);

        let missing = s.get_task(TaskId::new()).await.unwrap();
        assert!(missing.is_none());

        let list = s.list_tasks(10).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
    }

    #[tokio::test]
    async fn update_task_status_rewrites_column() {
        let s = Store::in_memory().unwrap();
        let id = TaskId::new();
        s.upsert_task(id, "g", &TaskStatus::Queued, 0).unwrap();

        let new_status = serde_json::json!({"phase": "Running"});
        s.update_task_status(id, new_status.clone()).await.unwrap();

        let r = s.get_task(id).await.unwrap().unwrap();
        assert_eq!(r.status, new_status);
    }

    #[tokio::test]
    async fn write_and_query_memory_filters() {
        let s = Store::in_memory().unwrap();
        s.write_memory(&MemoryRecord::Decision {
            title: "adopt HNSW".into(),
            body: "use usearch for retrieval".into(),
            task_id: None,
        })
        .await
        .unwrap();
        s.write_memory(&MemoryRecord::RejectedApproach {
            summary: "scan with linear cosine".into(),
            reason: "too slow above 10k slices".into(),
        })
        .await
        .unwrap();

        // No filters → all.
        let all = s.query_memory(None, None, 10).await.unwrap();
        assert_eq!(all.len(), 2);

        // Kind filter only.
        let decisions = s.query_memory(Some("Decision"), None, 10).await.unwrap();
        assert_eq!(decisions.len(), 1);

        // Topic filter only.
        let scan = s.query_memory(None, Some("scan"), 10).await.unwrap();
        assert_eq!(scan.len(), 1);

        // AND filter — both conditions must match.
        let both = s
            .query_memory(Some("Decision"), Some("HNSW"), 10)
            .await
            .unwrap();
        assert_eq!(both.len(), 1);

        let mismatch = s
            .query_memory(Some("Decision"), Some("scan"), 10)
            .await
            .unwrap();
        assert!(mismatch.is_empty());
    }

    #[tokio::test]
    async fn async_warm_crate_round_trip() {
        let s = Store::in_memory().unwrap();
        assert!(!s.is_crate_warm("c", "h").await.unwrap());
        s.mark_crate_warm("c", "h").await.unwrap();
        assert!(s.is_crate_warm("c", "h").await.unwrap());
        assert!(!s.is_crate_warm("c", "different").await.unwrap());
    }

    #[tokio::test]
    async fn evict_stale_warm_crates_deletes_old_rows() {
        let s = Store::in_memory().unwrap();
        s.mark_crate_warm("recent", "h").await.unwrap();
        // Stuff a row with a far-past timestamp.
        s.conn
            .execute(
                "INSERT INTO warm_crates (crate_name, last_checked_at, last_files_hash)
                 VALUES (?1, ?2, ?3)",
                params!["stale", 0_i64, "h"],
            )
            .unwrap();

        let deleted = s.evict_stale_warm_crates(WARM_TTL_SECS).await.unwrap();
        assert_eq!(deleted, 1);
        assert!(s.is_crate_warm("recent", "h").await.unwrap());
        assert!(!s.is_crate_warm("stale", "h").await.unwrap());
    }

    #[test]
    fn upsert_task_replaces_status() {
        let s = Store::in_memory().unwrap();
        let id = TaskId::new();
        s.upsert_task(id, "goal one", &TaskStatus::Queued, 0).unwrap();
        s.upsert_task(id, "goal one", &TaskStatus::Rejected, 100).unwrap();
        // No assertion on read-back (no getter yet); the test passes if
        // the second insert doesn't error on the PK conflict.
    }
}
