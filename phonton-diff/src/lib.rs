//! Atomic diff application and rollback via git2.
//!
//! Provides the [`DiffApplier`] which stages verified changes into the
//! git index without committing, and a [`RollbackGuard`] wrapper for
//! rolling back a goal's worth of changes on failure.

use anyhow::{anyhow, Context, Result};
use git2::{ObjectType, Repository, Signature, StashFlags};
use phonton_types::{Checkpoint, DiffHunk, DiffLine, SubtaskId, TaskId};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct DiffApplier {
    repo: Repository,
    stash_oid: Option<git2::Oid>,
}

impl DiffApplier {
    /// Open the repository at `path`. Returns a clear error if `path`
    /// is not inside a git repository.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let p = path.as_ref();
        let repo = Repository::discover(p).map_err(|e| {
            anyhow!(
                "'{}' is not inside a git repository (git2: {})",
                p.display(),
                e.message()
            )
        })?;
        Ok(Self {
            repo,
            stash_oid: None,
        })
    }

    /// Apply a set of verified hunks to the worktree and stage them.
    /// New files (no existing file, or `old_count == 0`) are written
    /// directly; modifications go through a `git2::Diff` so offsets are
    /// handled by libgit2's patch applier.
    pub fn apply_verified_hunks(&mut self, hunks: &[DiffHunk]) -> Result<()> {
        let workdir = self
            .repo
            .workdir()
            .ok_or_else(|| anyhow!("repository has no worktree (bare repo)"))?
            .to_path_buf();

        let mut grouped: BTreeMap<PathBuf, Vec<&DiffHunk>> = BTreeMap::new();
        for h in hunks {
            grouped.entry(h.file_path.clone()).or_default().push(h);
        }

        let mut new_files: Vec<PathBuf> = Vec::new();
        let mut modified: BTreeMap<PathBuf, Vec<&DiffHunk>> = BTreeMap::new();

        for (path, hs) in grouped {
            let full = workdir.join(&path);
            let treat_as_new =
                !full.exists() || hs.iter().all(|h| h.old_count == 0 && h.old_start == 0);
            if treat_as_new {
                let content = reconstruct_new_side(&hs);
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(&full, content)
                    .with_context(|| format!("writing new file {}", path.display()))?;
                new_files.push(path);
            } else {
                modified.insert(path, hs);
            }
        }

        if !modified.is_empty() {
            let diff_text = build_unified_diff(&modified);
            let diff = git2::Diff::from_buffer(diff_text.as_bytes())
                .context("constructing git2::Diff from unified patch text")?;
            self.repo
                .apply(&diff, git2::ApplyLocation::WorkDir, None)
                .context("git apply failed in worktree")?;
        }

        let mut index = self.repo.index()?;
        for path in new_files.iter().chain(modified.keys()) {
            index
                .add_path(path)
                .with_context(|| format!("staging {}", path.display()))?;
        }
        index.write()?;
        Ok(())
    }

    /// Stash all current worktree + index state (including untracked
    /// files) under `message` so it can be restored by [`rollback`].
    /// No-op if the worktree is clean.
    fn save_restore_point(&mut self, message: &str) -> Result<()> {
        let sig = self
            .repo
            .signature()
            .or_else(|_| Signature::now("phonton", "phonton@localhost"))?;
        match self.repo.stash_save(
            &sig,
            message,
            Some(StashFlags::INCLUDE_UNTRACKED | StashFlags::KEEP_INDEX),
        ) {
            Ok(oid) => {
                self.stash_oid = Some(oid);
                Ok(())
            }
            Err(e) if e.code() == git2::ErrorCode::NotFound => {
                self.stash_oid = None;
                Ok(())
            }
            Err(e) => Err(anyhow!("stash_save failed: {}", e)),
        }
    }

    /// Roll back worktree + index to the pre-apply state.
    pub fn rollback(&mut self) -> Result<()> {
        let mut co = git2::build::CheckoutBuilder::new();
        co.force().remove_untracked(true);
        self.repo
            .checkout_head(Some(&mut co))
            .context("checkout_head during rollback")?;

        if self.stash_oid.take().is_some() {
            self.repo
                .stash_pop(0, None)
                .context("stash_pop during rollback")?;
        }
        Ok(())
    }

    /// Borrow the underlying repository (for tests / advanced callers).
    pub fn repo(&self) -> &Repository {
        &self.repo
    }

    /// Create a point-in-time checkpoint commit for a subtask that
    /// just passed verify.
    ///
    /// The commit is written on a side ref under
    /// `refs/phonton/checkpoints/<task_id>/<seq>` so HEAD's
    /// user-visible history isn't polluted, but the worktree state at
    /// the moment of the checkpoint is fully reproducible: the commit
    /// is a real `git2::Commit` object whose tree captures everything
    /// currently in the index.
    ///
    /// The checkpoint's parent is the current HEAD, so a `git log`
    /// rooted at the checkpoint shows the user's pre-Phonton history
    /// followed by Phonton's chain of subtask commits.
    pub fn commit_checkpoint(
        &mut self,
        task_id: TaskId,
        subtask_id: SubtaskId,
        seq: u32,
        message: &str,
    ) -> Result<Checkpoint> {
        // Stage everything in the worktree so the checkpoint snapshots
        // the *whole* current state, not just the last apply.
        let mut index = self.repo.index()?;
        index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)?;
        index.write()?;
        let tree_oid = index.write_tree()?;
        let tree = self.repo.find_tree(tree_oid)?;

        let sig = self
            .repo
            .signature()
            .or_else(|_| Signature::now("phonton", "phonton@localhost"))?;

        // Parent = current HEAD if it exists; otherwise the checkpoint
        // is a root commit (fresh repo case).
        let parents: Vec<git2::Commit> = match self.repo.head() {
            Ok(head_ref) => {
                if let Ok(obj) = head_ref.peel(ObjectType::Commit) {
                    if let Ok(c) = obj.into_commit() {
                        vec![c]
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            }
            Err(_) => Vec::new(),
        };
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();

        let full_msg = format!(
            "phonton:checkpoint task={} subtask={} seq={}\n\n{}",
            task_id, subtask_id, seq, message
        );
        let oid = self
            .repo
            .commit(None, &sig, &sig, &full_msg, &tree, &parent_refs)
            .context("git2::Repository::commit for checkpoint")?;

        // Move the side ref to the new commit. Force-update so re-runs
        // of the same (task, seq) overwrite the prior pointer.
        let ref_name = format!("refs/phonton/checkpoints/{task_id}/{seq}");
        self.repo
            .reference(&ref_name, oid, true, &full_msg)
            .with_context(|| format!("creating checkpoint ref {ref_name}"))?;

        Ok(Checkpoint {
            task_id,
            subtask_id,
            seq,
            commit_oid: oid.to_string(),
            message: message.chars().take(120).collect(),
            timestamp_ms: now_ms(),
        })
    }

    /// List every checkpoint recorded for `task_id`, ordered by `seq`
    /// ascending. Returns empty if no checkpoints have been taken.
    pub fn list_checkpoints(&self, task_id: TaskId) -> Result<Vec<Checkpoint>> {
        let prefix = format!("refs/phonton/checkpoints/{task_id}/");
        let mut out: Vec<Checkpoint> = Vec::new();
        let refs = self.repo.references()?;
        for r in refs.flatten() {
            let Some(name) = r.name() else { continue };
            let Some(rest) = name.strip_prefix(&prefix) else {
                continue;
            };
            let Ok(seq) = rest.parse::<u32>() else { continue };
            let Some(oid) = r.target() else { continue };
            let commit = self.repo.find_commit(oid)?;
            let summary = commit.summary().unwrap_or("").to_string();
            // Subtask id is recorded only in the commit message; we
            // surface a placeholder here since rollback only needs seq
            // + commit oid. Callers that retain the full Checkpoint at
            // creation time should prefer that copy.
            out.push(Checkpoint {
                task_id,
                subtask_id: SubtaskId::default(),
                seq,
                commit_oid: oid.to_string(),
                message: summary,
                timestamp_ms: (commit.time().seconds() as u64) * 1_000,
            });
        }
        out.sort_by_key(|c| c.seq);
        Ok(out)
    }

    /// Hard-reset the worktree + index to the commit named by
    /// `commit_oid`. Used by the orchestrator to perform a "Rollback to
    /// step N" request: the user gives up everything since the named
    /// checkpoint, in exchange for a clean replay starting from there.
    ///
    /// The rollback is destructive — uncommitted worktree changes are
    /// discarded. Call this only after the orchestrator has aborted
    /// every in-flight worker.
    pub fn rollback_to_checkpoint(&mut self, commit_oid: &str) -> Result<()> {
        let oid = git2::Oid::from_str(commit_oid)
            .with_context(|| format!("parsing checkpoint oid {commit_oid}"))?;
        let obj = self
            .repo
            .find_object(oid, Some(ObjectType::Commit))
            .context("checkpoint commit not found")?;
        let mut co = git2::build::CheckoutBuilder::new();
        co.force().remove_untracked(true);
        self.repo
            .reset(&obj, git2::ResetType::Hard, Some(&mut co))
            .context("hard reset to checkpoint")?;
        Ok(())
    }

    /// Save a restore point, apply `hunks`, and return a guard that
    /// will roll back on drop unless [`RollbackGuard::commit`] is called.
    pub async fn apply_transaction(
        &mut self,
        hunks: Vec<DiffHunk>,
        task_id: &str,
    ) -> Result<RollbackGuard<'_>> {
        self.save_restore_point(&format!("phonton:{}", task_id))?;
        if let Err(e) = self.apply_verified_hunks(&hunks) {
            let _ = self.rollback();
            return Err(e);
        }
        Ok(RollbackGuard {
            applier: Some(self),
            committed: false,
        })
    }
}

/// RAII guard returned by [`DiffApplier::apply_transaction`]. Drops back
/// to the pre-apply state unless [`commit`](Self::commit) is called.
pub struct RollbackGuard<'a> {
    applier: Option<&'a mut DiffApplier>,
    committed: bool,
}

impl<'a> RollbackGuard<'a> {
    /// Keep the applied changes and discard the saved restore point.
    pub fn commit(mut self) -> Result<()> {
        self.committed = true;
        if let Some(app) = self.applier.as_mut() {
            if app.stash_oid.take().is_some() {
                app.repo
                    .stash_drop(0)
                    .context("stash_drop during commit")?;
            }
        }
        Ok(())
    }
}

impl<'a> Drop for RollbackGuard<'a> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Some(app) = self.applier.as_mut() {
            let _ = app.rollback();
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn build_unified_diff(by_file: &BTreeMap<PathBuf, Vec<&DiffHunk>>) -> String {
    let mut out = String::new();
    for (path, hunks) in by_file {
        let p = path.to_string_lossy().replace('\\', "/");
        out.push_str(&format!("--- a/{}\n", p));
        out.push_str(&format!("+++ b/{}\n", p));
        for h in hunks {
            out.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                h.old_start, h.old_count, h.new_start, h.new_count
            ));
            for line in &h.lines {
                match line {
                    DiffLine::Context(s) => {
                        out.push(' ');
                        out.push_str(s);
                        out.push('\n');
                    }
                    DiffLine::Added(s) => {
                        out.push('+');
                        out.push_str(s);
                        out.push('\n');
                    }
                    DiffLine::Removed(s) => {
                        out.push('-');
                        out.push_str(s);
                        out.push('\n');
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::{SubtaskId, TaskId};

    fn init_repo_with_seed(dir: &Path) -> Repository {
        let repo = Repository::init(dir).unwrap();
        // Seed commit so HEAD exists.
        std::fs::write(dir.join("seed.txt"), "seed\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("seed.txt")).unwrap();
        idx.write().unwrap();
        let tree_oid = idx.write_tree().unwrap();
        let sig = Signature::now("phonton-test", "test@phonton").unwrap();
        {
            // Scope the tree borrow so it's dropped before we return repo.
            let tree = repo.find_tree(tree_oid).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "seed", &tree, &[])
                .unwrap();
        }
        repo
    }

    #[test]
    fn checkpoint_round_trip_lists_and_rolls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let _repo = init_repo_with_seed(tmp.path());
        let mut applier = DiffApplier::open(tmp.path()).unwrap();
        let task = TaskId::new();

        // Take checkpoint #1 with one file.
        std::fs::write(tmp.path().join("a.txt"), "alpha\n").unwrap();
        let cp1 = applier
            .commit_checkpoint(task, SubtaskId::new(), 1, "after subtask 1")
            .unwrap();
        assert_eq!(cp1.seq, 1);

        // Take checkpoint #2 after a second change.
        std::fs::write(tmp.path().join("a.txt"), "alpha+beta\n").unwrap();
        let cp2 = applier
            .commit_checkpoint(task, SubtaskId::new(), 2, "after subtask 2")
            .unwrap();
        assert_eq!(cp2.seq, 2);
        assert_ne!(cp1.commit_oid, cp2.commit_oid);

        // List should return both, ordered by seq.
        let listed = applier.list_checkpoints(task).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].seq, 1);
        assert_eq!(listed[1].seq, 2);

        // Rollback to #1: a.txt should revert to "alpha".
        applier.rollback_to_checkpoint(&cp1.commit_oid).unwrap();
        let after = std::fs::read_to_string(tmp.path().join("a.txt")).unwrap();
        assert_eq!(after.replace("\r\n", "\n"), "alpha\n");
    }

    #[test]
    fn list_checkpoints_empty_when_none_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let _repo = init_repo_with_seed(tmp.path());
        let applier = DiffApplier::open(tmp.path()).unwrap();
        let task = TaskId::new();
        let listed = applier.list_checkpoints(task).unwrap();
        assert!(listed.is_empty());
    }
}

fn reconstruct_new_side(hunks: &[&DiffHunk]) -> String {
    let mut out = String::new();
    for h in hunks {
        for line in &h.lines {
            match line {
                DiffLine::Context(s) | DiffLine::Added(s) => {
                    out.push_str(s);
                    out.push('\n');
                }
                DiffLine::Removed(_) => {}
            }
        }
    }
    out
}
