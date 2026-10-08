//! Exact diff application and checkpoint history via git2.
//!
//! Provides the [`DiffApplier`] which stages verified changes into the
//! git index without committing. Legacy checkpoint rollback is disabled;
//! local Apply uses its own path-scoped journal for recovery.

use anyhow::{anyhow, Context, Result};
use git2::{ObjectType, Repository, Signature};
use phonton_types::{Checkpoint, DiffHunk, SubtaskId, TaskId};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct DiffApplier {
    repo: Repository,
    working_dir: PathBuf,
    repo_prefix: PathBuf,
}

/// Apply exact hunks to a directory without a Git index. Validation completes
/// before any file write; I/O failures propagate to the caller. This is not a
/// multi-file transaction and must not be used as an automatic rollback path.
pub fn apply_worktree_hunks(root: &Path, hunks: &[DiffHunk]) -> Result<()> {
    let allowed: Vec<_> = hunks.iter().map(|h| h.file_path.clone()).collect();
    let contents = phonton_local::edit::materialize_hunks_with_new_files(root, &allowed, hunks)?;
    for (path, content) in contents {
        let full = root.join(&path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(full, content)
            .with_context(|| format!("writing exact candidate {}", path.display()))?;
    }
    Ok(())
}

impl DiffApplier {
    /// Open the repository at `path`. Returns a clear error if `path`
    /// is not inside a git repository.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let p = path.as_ref();
        let working_dir = std::fs::canonicalize(p)
            .with_context(|| format!("opening working directory {}", p.display()))?;
        if !working_dir.is_dir() {
            return Err(anyhow!("'{}' is not a working directory", p.display()));
        }
        let repo = Repository::discover(&working_dir).map_err(|e| {
            anyhow!(
                "'{}' is not inside a git repository (git2: {})",
                p.display(),
                e.message()
            )
        })?;
        let repo_root = std::fs::canonicalize(
            repo.workdir()
                .ok_or_else(|| anyhow!("repository has no worktree (bare repo)"))?,
        )?;
        let repo_prefix = working_dir
            .strip_prefix(&repo_root)
            .with_context(|| {
                format!(
                    "working directory {} is outside repository {}",
                    working_dir.display(),
                    repo_root.display()
                )
            })?
            .to_path_buf();
        Ok(Self {
            repo,
            working_dir,
            repo_prefix,
        })
    }

    /// Resolve a path relative to the selected working directory into a
    /// repository-root path for Git index and checkpoint operations.
    pub fn repository_relative_path(&self, path: &Path) -> Result<PathBuf> {
        let safe =
            phonton_local::edit::safe_relative_path(&path.to_string_lossy().replace('\\', "/"))?;
        Ok(self.repo_prefix.join(safe))
    }

    /// Revalidate every hunk against the exact current worktree, then apply
    /// and stage only those paths. Missing files can be created; insertions
    /// into existing files never silently replace the file. Validation uses
    /// the same strict materializer as verification and local candidates.
    pub fn apply_verified_hunks(&mut self, hunks: &[DiffHunk]) -> Result<()> {
        let allowed: Vec<_> = hunks.iter().map(|h| h.file_path.clone()).collect();
        let contents = phonton_local::edit::materialize_hunks_with_new_files(
            &self.working_dir,
            &allowed,
            hunks,
        )?;
        // Obtain the index before writes, so an unavailable index cannot leave
        // partially applied work. All hunks have already passed validation.
        let mut index = self.repo.index()?;
        for (path, content) in &contents {
            let full = self.working_dir.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&full, content)
                .with_context(|| format!("writing exact candidate {}", path.display()))?;
            index
                .add_path(&self.repository_relative_path(path)?)
                .with_context(|| format!("staging {}", path.display()))?;
        }
        index.write()?;
        Ok(())
    }

    /// Remember how `paths` look before a task first touches them: their
    /// worktree bytes and index entry, or that they were absent. Call before
    /// [`Self::apply_verified_hunks`]; paths already recorded for `task_id`
    /// keep their first, pre-task snapshot. [`Self::revert_task`] restores
    /// this snapshot, so edits the user made before the task survive a reject.
    pub fn record_pre_task_state(&mut self, task_id: TaskId, paths: &[PathBuf]) -> Result<()> {
        let ref_name = pre_task_ref(task_id);
        let previous = self
            .repo
            .find_reference(&ref_name)
            .ok()
            .and_then(|r| r.peel_to_commit().ok());
        let mut recorded = previous
            .as_ref()
            .map(|c| pre_task_manifest(c.message().unwrap_or_default()))
            .unwrap_or_default();
        let mut snapshot = git2::Index::new()?;
        if let Some(commit) = &previous {
            snapshot.read_tree(&commit.tree()?)?;
        }
        let mut live = self.repo.index()?;
        live.read(true)?;
        let mut added = false;
        for path in paths {
            let rel = repo_path_string(&self.repository_relative_path(path)?);
            if !recorded.insert(rel.clone()) {
                continue;
            }
            added = true;
            let staged = live.get_path(Path::new(&rel), 0);
            if let Ok(bytes) = std::fs::read(self.working_dir.join(path)) {
                let mode = staged.as_ref().map_or(0o100644, |e| e.mode);
                snapshot.add(&snapshot_entry(
                    &format!("w/{rel}"),
                    self.repo.blob(&bytes)?,
                    mode,
                ))?;
            }
            if let Some(entry) = staged {
                snapshot.add(&snapshot_entry(&format!("i/{rel}"), entry.id, entry.mode))?;
            }
        }
        if !added {
            return Ok(());
        }
        let tree = self.repo.find_tree(snapshot.write_tree_to(&self.repo)?)?;
        let sig = self
            .repo
            .signature()
            .or_else(|_| Signature::now("phonton", "phonton@localhost"))?;
        let manifest: Vec<&str> = recorded.iter().map(String::as_str).collect();
        let message = format!("phonton:pretask task={task_id}\n\n{}", manifest.join("\n"));
        let parents: Vec<&git2::Commit> = previous.iter().collect();
        let oid = self
            .repo
            .commit(None, &sig, &sig, &message, &tree, &parents)?;
        self.repo
            .reference(&ref_name, oid, true, "phonton pre-task state")?;
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
    /// the moment of the checkpoint is reproducible: the commit is a real
    /// `git2::Commit` object whose tree starts at the preceding checkpoint
    /// (or HEAD for the first step) and adds only `verified_paths`. The
    /// verified paths were staged by [`Self::apply_verified_hunks`]. Unrelated
    /// staged and worktree changes are excluded from the side-ref tree.
    ///
    /// Each checkpoint's parent is the previous checkpoint, or current HEAD
    /// for the first step, so its tree includes prior verified subtasks.
    pub fn commit_checkpoint(
        &mut self,
        task_id: TaskId,
        subtask_id: SubtaskId,
        seq: u32,
        message: &str,
        verified_paths: &[std::path::PathBuf],
    ) -> Result<Checkpoint> {
        if seq == 0 || verified_paths.is_empty() {
            return Err(anyhow!(
                "checkpoint requires a positive step and verified paths"
            ));
        }
        let mut live_index = self.repo.index()?;
        live_index.read(true)?;

        let sig = self
            .repo
            .signature()
            .or_else(|_| Signature::now("phonton", "phonton@localhost"))?;

        // The first checkpoint roots at HEAD; later steps use the preceding
        // side-ref so unrelated pre-staged index entries never enter the tree.
        let parents: Vec<git2::Commit> = if seq > 1 {
            let previous = format!("refs/phonton/checkpoints/{task_id}/{}", seq - 1);
            vec![self
                .repo
                .find_reference(&previous)
                .with_context(|| format!("previous checkpoint {previous} missing"))?
                .peel_to_commit()?]
        } else {
            match self.repo.head() {
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
            }
        };
        let mut checkpoint_index = git2::Index::new()?;
        if let Some(parent) = parents.first() {
            checkpoint_index.read_tree(&parent.tree()?)?;
        }
        let mut paths = BTreeSet::new();
        for path in verified_paths {
            paths.insert(self.repository_relative_path(path)?);
        }
        for path in paths {
            let entry = live_index.get_path(&path, 0).ok_or_else(|| {
                anyhow!("verified checkpoint path {} is not staged", path.display())
            })?;
            checkpoint_index.add(&entry)?;
        }
        let tree_oid = checkpoint_index.write_tree_to(&self.repo)?;
        let tree = self.repo.find_tree(tree_oid)?;
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
            let Ok(seq) = rest.parse::<u32>() else {
                continue;
            };
            let Some(oid) = r.target() else { continue };
            let commit = self.repo.find_commit(oid)?;
            let summary = commit.summary().unwrap_or("").to_string();
            // Subtask id is recorded only in the commit message; we
            // surface a placeholder here. Callers that retain the full Checkpoint at
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

    /// Undo a task's verified edits: every path its checkpoints changed goes
    /// back to its pre-task content in both the worktree and the index.
    /// Refuses before writing anything if any of those paths changed after
    /// the task staged them, so unrelated or later work is never discarded.
    /// Returns the repository-relative paths that were restored.
    pub fn revert_task(&mut self, task_id: TaskId) -> Result<Vec<PathBuf>> {
        let checkpoints = self.list_checkpoints(task_id)?;
        let (Some(first), Some(last)) = (checkpoints.first(), checkpoints.last()) else {
            return Ok(Vec::new());
        };
        let first = self
            .repo
            .find_commit(git2::Oid::from_str(&first.commit_oid)?)?;
        let last = self
            .repo
            .find_commit(git2::Oid::from_str(&last.commit_oid)?)?;
        let base_tree = match first.parent(0) {
            Ok(parent) => Some(parent.tree()?),
            Err(_) => None,
        };
        let final_tree = last.tree()?;
        let diff = self
            .repo
            .diff_tree_to_tree(base_tree.as_ref(), Some(&final_tree), None)?;

        let mut index = self.repo.index()?;
        index.read(true)?;
        let mut restore = Vec::new();
        let mut remove = Vec::new();
        for delta in diff.deltas() {
            let Some(path) = delta.new_file().path().or(delta.old_file().path()) else {
                continue;
            };
            let path = path.to_path_buf();
            let staged = index.get_path(&path, 0).map(|e| e.id);
            let expected = delta.new_file().exists().then(|| delta.new_file().id());
            let status = self.repo.status_file(&path).unwrap_or(git2::Status::WT_NEW);
            let worktree_dirty = status.intersects(
                git2::Status::WT_MODIFIED
                    | git2::Status::WT_DELETED
                    | git2::Status::WT_NEW
                    | git2::Status::WT_TYPECHANGE
                    | git2::Status::WT_RENAMED,
            );
            if staged != expected || (expected.is_some() && worktree_dirty) {
                return Err(anyhow!(
                    "{} changed after the task; nothing was reverted. Resolve it by hand.",
                    path.display()
                ));
            }
            if delta.old_file().exists() {
                restore.push(path);
            } else {
                remove.push(path);
            }
        }

        // Paths with a recorded pre-task snapshot go back to exactly that
        // worktree and index state; others fall back to the first checkpoint's
        // parent (tasks recorded before snapshots existed).
        let pre_task = self
            .repo
            .find_reference(&pre_task_ref(task_id))
            .ok()
            .and_then(|r| r.peel_to_commit().ok());
        let workdir = self
            .repo
            .workdir()
            .ok_or_else(|| anyhow!("repository has no worktree (bare repo)"))?
            .to_path_buf();
        let mut snapshotted = Vec::new();
        if let Some(commit) = &pre_task {
            let recorded = pre_task_manifest(commit.message().unwrap_or_default());
            let tree = commit.tree()?;
            for path in restore.iter().chain(remove.iter()) {
                let rel = repo_path_string(path);
                if !recorded.contains(&rel) {
                    continue;
                }
                let full = workdir.join(path);
                match tree.get_path(Path::new(&format!("w/{rel}"))) {
                    Ok(entry) => {
                        let blob = self.repo.find_blob(entry.id())?;
                        if let Some(parent) = full.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(&full, blob.content())?;
                    }
                    Err(_) => match std::fs::remove_file(&full) {
                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                        _ => {}
                    },
                }
                match tree.get_path(Path::new(&format!("i/{rel}"))) {
                    Ok(entry) => {
                        index.add(&snapshot_entry(&rel, entry.id(), entry.filemode() as u32))?
                    }
                    Err(_) => {
                        let _ = index.remove_path(path);
                    }
                }
                snapshotted.push(path.clone());
            }
        }
        index.write()?;
        let restored: Vec<PathBuf> = snapshotted.clone();
        restore.retain(|p| !snapshotted.contains(p));
        remove.retain(|p| !snapshotted.contains(p));

        if let Some(base) = &base_tree {
            if !restore.is_empty() {
                let mut co = git2::build::CheckoutBuilder::new();
                co.force();
                for path in &restore {
                    co.path(path);
                }
                self.repo.checkout_tree(base.as_object(), Some(&mut co))?;
            }
        }
        index.read(true)?;
        for path in &remove {
            index.remove_path(path)?;
            match std::fs::remove_file(workdir.join(path)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        index.write()?;
        restore.extend(remove);
        restore.extend(restored);
        Ok(restore)
    }

    /// Refuse legacy checkpoint rollback until it can restore only owned
    /// paths. The former hard reset moved HEAD and deleted unrelated work.
    /// Local Apply journals use a separate scoped rollback implementation.
    pub fn rollback_to_checkpoint(&mut self, _commit_oid: &str) -> Result<()> {
        Err(anyhow!(
            "Legacy checkpoint rollback is disabled: it could discard unrelated work. Use a local Apply journal for scoped rollback."
        ))
    }
}

fn pre_task_ref(task_id: TaskId) -> String {
    format!("refs/phonton/pretask/{task_id}")
}

/// Recorded paths, one per line after the pre-task commit's first blank line.
fn pre_task_manifest(message: &str) -> BTreeSet<String> {
    message
        .split_once("\n\n")
        .map(|(_, body)| {
            body.lines()
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Git paths use `/` on every platform.
fn repo_path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn snapshot_entry(path: &str, id: git2::Oid, mode: u32) -> git2::IndexEntry {
    git2::IndexEntry {
        ctime: git2::IndexTime::new(0, 0),
        mtime: git2::IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        file_size: 0,
        id,
        flags: path.len().min(0xfff) as u16,
        flags_extended: 0,
        path: path.as_bytes().to_vec(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::{DiffLine, SubtaskId, TaskId};
    use std::path::PathBuf;

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
    fn nested_working_directory_applies_and_checkpoints_only_nested_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_repo_with_seed(tmp.path());
        let nested = tmp.path().join("project");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(tmp.path().join("code.txt"), "outer\n").unwrap();
        std::fs::write(nested.join("code.txt"), "before\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("code.txt")).unwrap();
        index.add_path(Path::new("project/code.txt")).unwrap();
        index.write().unwrap();
        let hunk = DiffHunk {
            file_path: "code.txt".into(),
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![
                DiffLine::Removed("before".into()),
                DiffLine::Added("verified".into()),
            ],
        };
        let mut applier = DiffApplier::open(&nested).unwrap();
        applier.apply_verified_hunks(&[hunk]).unwrap();
        assert_eq!(
            std::fs::read_to_string(nested.join("code.txt")).unwrap(),
            "verified\n"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("code.txt")).unwrap(),
            "outer\n"
        );
        let checkpoint = applier
            .commit_checkpoint(
                TaskId::new(),
                SubtaskId::new(),
                1,
                "verified",
                &["code.txt".into()],
            )
            .unwrap();
        let tree = repo
            .find_commit(git2::Oid::from_str(&checkpoint.commit_oid).unwrap())
            .unwrap()
            .tree()
            .unwrap();
        assert!(tree.get_path(Path::new("project/code.txt")).is_ok());
        assert!(tree.get_path(Path::new("code.txt")).is_err());
    }

    #[test]
    fn revert_task_restores_owned_paths_and_refuses_after_later_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_repo_with_seed(tmp.path());
        std::fs::write(tmp.path().join("mine.txt"), "untouched\n").unwrap();
        let edit_seed = DiffHunk {
            file_path: "seed.txt".into(),
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![
                DiffLine::Removed("seed".into()),
                DiffLine::Added("changed".into()),
            ],
        };
        let create = DiffHunk {
            file_path: "new.txt".into(),
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine::Added("fresh".into())],
        };
        let paths: Vec<PathBuf> = vec!["seed.txt".into(), "new.txt".into()];

        // A later user edit to an owned path blocks the revert entirely.
        let mut applier = DiffApplier::open(tmp.path()).unwrap();
        let task = TaskId::new();
        applier
            .apply_verified_hunks(&[edit_seed.clone(), create.clone()])
            .unwrap();
        applier
            .commit_checkpoint(task, SubtaskId::new(), 1, "edit", &paths)
            .unwrap();
        std::fs::write(tmp.path().join("seed.txt"), "user edit\n").unwrap();
        assert!(applier.revert_task(task).is_err());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("seed.txt")).unwrap(),
            "user edit\n"
        );
        assert!(tmp.path().join("new.txt").exists());

        // Untouched since the task: both paths go back, unrelated work stays.
        std::fs::write(tmp.path().join("seed.txt"), "changed\n").unwrap();
        let reverted = applier.revert_task(task).unwrap();
        assert_eq!(reverted.len(), 2);
        // Checkout honours core.autocrlf, exactly like `git restore`.
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("seed.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "seed\n"
        );
        assert!(!tmp.path().join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("mine.txt")).unwrap(),
            "untouched\n"
        );
        let statuses = repo.statuses(None).unwrap();
        let dirty: Vec<_> = statuses
            .iter()
            .filter_map(|s| s.path().map(str::to_string))
            .collect();
        assert_eq!(dirty, vec!["mine.txt".to_string()]);
    }

    #[test]
    fn revert_keeps_edits_and_untracked_files_that_predate_the_task() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_repo_with_seed(tmp.path());
        // Before the task: an unstaged edit to a tracked file and an
        // untracked file, both of which the task then changes.
        std::fs::write(tmp.path().join("seed.txt"), "user\n").unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "draft\n").unwrap();
        let hunk = |path: &str, old: &str, new: &str| DiffHunk {
            file_path: path.into(),
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine::Removed(old.into()), DiffLine::Added(new.into())],
        };
        let paths: Vec<PathBuf> = vec!["seed.txt".into(), "notes.txt".into()];
        let mut applier = DiffApplier::open(tmp.path()).unwrap();
        let task = TaskId::new();
        applier.record_pre_task_state(task, &paths).unwrap();
        applier
            .apply_verified_hunks(&[
                hunk("seed.txt", "user", "task"),
                hunk("notes.txt", "draft", "task"),
            ])
            .unwrap();
        applier
            .commit_checkpoint(task, SubtaskId::new(), 1, "edit", &paths)
            .unwrap();

        assert_eq!(applier.revert_task(task).unwrap().len(), 2);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("seed.txt")).unwrap(),
            "user\n"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("notes.txt")).unwrap(),
            "draft\n"
        );
        let status = |p: &str| repo.status_file(Path::new(p)).unwrap();
        assert_eq!(status("seed.txt"), git2::Status::WT_MODIFIED);
        assert_eq!(status("notes.txt"), git2::Status::WT_NEW);
    }

    #[test]
    fn checkpoint_round_trip_lists_and_rolls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_repo_with_seed(tmp.path());
        let mut applier = DiffApplier::open(tmp.path()).unwrap();
        let task = TaskId::new();

        // Take checkpoint #1 after a verified edit stages one file.
        applier
            .apply_verified_hunks(&[DiffHunk {
                file_path: "a.txt".into(),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 1,
                lines: vec![DiffLine::Added("alpha".into())],
            }])
            .unwrap();
        let cp1 = applier
            .commit_checkpoint(
                task,
                SubtaskId::new(),
                1,
                "after subtask 1",
                &["a.txt".into()],
            )
            .unwrap();
        assert_eq!(cp1.seq, 1);

        // Take checkpoint #2 after a second verified edit.
        applier
            .apply_verified_hunks(&[DiffHunk {
                file_path: "a.txt".into(),
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![
                    DiffLine::Removed("alpha".into()),
                    DiffLine::Added("alpha+beta".into()),
                ],
            }])
            .unwrap();
        let cp2 = applier
            .commit_checkpoint(
                task,
                SubtaskId::new(),
                2,
                "after subtask 2",
                &["a.txt".into()],
            )
            .unwrap();
        assert_eq!(cp2.seq, 2);
        assert_ne!(cp1.commit_oid, cp2.commit_oid);
        let second = repo
            .find_commit(git2::Oid::from_str(&cp2.commit_oid).unwrap())
            .unwrap();
        assert_eq!(second.parent_id(0).unwrap().to_string(), cp1.commit_oid);

        // List should return both, ordered by seq.
        let listed = applier.list_checkpoints(task).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].seq, 1);
        assert_eq!(listed[1].seq, 2);

        // Legacy checkpoint rollback must leave all user and Phonton bytes
        // intact until it has a path-scoped recovery protocol.
        std::fs::write(tmp.path().join("a.txt"), "user's later edit\n").unwrap();
        std::fs::write(tmp.path().join("untracked.txt"), "private notes\n").unwrap();
        std::fs::write(tmp.path().join("staged.txt"), "staged work\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("staged.txt")).unwrap();
        index.write().unwrap();
        let index_before = std::fs::read(repo.path().join("index")).unwrap();
        let head_before = repo.head().unwrap().target().unwrap();
        assert!(applier
            .rollback_to_checkpoint(&cp1.commit_oid)
            .unwrap_err()
            .to_string()
            .contains("disabled"));
        assert_eq!(repo.head().unwrap().target().unwrap(), head_before);
        assert_eq!(
            std::fs::read(repo.path().join("index")).unwrap(),
            index_before
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "user's later edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("untracked.txt")).unwrap(),
            "private notes\n"
        );
    }

    #[test]
    fn checkpoint_does_not_stage_or_capture_unrelated_worktree_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_repo_with_seed(tmp.path());
        std::fs::write(tmp.path().join("seed.txt"), "user's unstaged edit\n").unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "user's untracked file\n").unwrap();
        std::fs::write(tmp.path().join("staged.txt"), "user's staged file\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("staged.txt")).unwrap();
        index.write().unwrap();
        let staged_oid = index.get_path(Path::new("staged.txt"), 0).unwrap().id;
        let baseline_seed = repo
            .index()
            .unwrap()
            .get_path(Path::new("seed.txt"), 0)
            .unwrap()
            .id;

        let mut applier = DiffApplier::open(tmp.path()).unwrap();
        applier
            .apply_verified_hunks(&[DiffHunk {
                file_path: "result.txt".into(),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 1,
                lines: vec![DiffLine::Added("verified".into())],
            }])
            .unwrap();
        let checkpoint = applier
            .commit_checkpoint(
                TaskId::new(),
                SubtaskId::new(),
                1,
                "verified result",
                &["result.txt".into()],
            )
            .unwrap();

        let mut index = repo.index().unwrap();
        index.read(true).unwrap();
        assert_eq!(
            index.get_path(Path::new("seed.txt"), 0).unwrap().id,
            baseline_seed
        );
        assert!(index.get_path(Path::new("notes.txt"), 0).is_none());
        assert!(index.get_path(Path::new("result.txt"), 0).is_some());
        assert_eq!(
            index.get_path(Path::new("staged.txt"), 0).unwrap().id,
            staged_oid
        );
        let tree = repo
            .find_commit(git2::Oid::from_str(&checkpoint.commit_oid).unwrap())
            .unwrap()
            .tree()
            .unwrap();
        assert!(tree.get_path(Path::new("notes.txt")).is_err());
        assert!(tree.get_path(Path::new("staged.txt")).is_err());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("seed.txt")).unwrap(),
            "user's unstaged edit\n"
        );
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

    #[test]
    fn invalid_second_file_leaves_worktree_and_index_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_repo_with_seed(tmp.path());
        std::fs::write(tmp.path().join("unrelated.txt"), "private work\n").unwrap();
        let index_before = std::fs::read(repo.path().join("index")).unwrap();
        let mut applier = DiffApplier::open(tmp.path()).unwrap();
        let hunks = vec![
            DiffHunk {
                file_path: "a-new.txt".into(),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 1,
                lines: vec![DiffLine::Added("new".into())],
            },
            DiffHunk {
                file_path: "seed.txt".into(),
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![
                    DiffLine::Removed("imagined".into()),
                    DiffLine::Added("changed".into()),
                ],
            },
        ];
        assert!(applier.apply_verified_hunks(&hunks).is_err());
        assert!(!tmp.path().join("a-new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("seed.txt")).unwrap(),
            "seed\n"
        );
        assert_eq!(
            std::fs::read(repo.path().join("index")).unwrap(),
            index_before
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("unrelated.txt")).unwrap(),
            "private work\n"
        );
    }

    #[test]
    fn insertion_preserves_existing_file_and_unrelated_index_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_repo_with_seed(tmp.path());
        std::fs::write(tmp.path().join("staged.txt"), "staged work\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("staged.txt")).unwrap();
        index.write().unwrap();
        let staged_oid = index.get_path(Path::new("staged.txt"), 0).unwrap().id;
        let mut applier = DiffApplier::open(tmp.path()).unwrap();
        applier
            .apply_verified_hunks(&[DiffHunk {
                file_path: "seed.txt".into(),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 1,
                lines: vec![DiffLine::Added("inserted".into())],
            }])
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("seed.txt")).unwrap(),
            "inserted\nseed\n"
        );
        let mut index = repo.index().unwrap();
        index.read(true).unwrap();
        assert_eq!(
            index.get_path(Path::new("staged.txt"), 0).unwrap().id,
            staged_oid
        );
        let applied_blob = repo
            .find_blob(index.get_path(Path::new("seed.txt"), 0).unwrap().id)
            .unwrap();
        assert_eq!(applied_blob.content(), b"inserted\nseed\n");
    }

    #[test]
    fn apply_verified_hunks_rejects_invented_old_side_without_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let _repo = init_repo_with_seed(tmp.path());
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/App.tsx"), "stale old line\n").unwrap();

        let mut applier = DiffApplier::open(tmp.path()).unwrap();
        let hunks = vec![DiffHunk {
            file_path: PathBuf::from("src/App.tsx"),
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 3,
            lines: vec![
                DiffLine::Removed("different old line".into()),
                DiffLine::Added("export function App() {".into()),
                DiffLine::Added("  return <main>Chess</main>;".into()),
                DiffLine::Added("}".into()),
            ],
        }];

        assert!(applier.apply_verified_hunks(&hunks).is_err());

        let written = std::fs::read_to_string(tmp.path().join("src/App.tsx")).unwrap();
        assert_eq!(written.replace("\r\n", "\n"), "stale old line\n");
    }

    #[test]
    fn apply_verified_hunks_applies_contextual_file_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let _repo = init_repo_with_seed(tmp.path());
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/config.js"),
            "export function loadConfig(raw = {}, env = process.env) {\n  const provider = raw.provider || env.PROVIDER || \"openai\";\n  return provider.trim();\n}\n",
        )
        .unwrap();

        let mut applier = DiffApplier::open(tmp.path()).unwrap();
        let hunks = vec![DiffHunk {
            file_path: PathBuf::from("src/config.js"),
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 7,
            lines: vec![
                DiffLine::Context(
                    "export function loadConfig(raw = {}, env = process.env) {".into(),
                ),
                DiffLine::Removed(
                    "  const provider = raw.provider || env.PROVIDER || \"openai\";".into(),
                ),
                DiffLine::Added(
                    "  const provider = raw.provider ?? env.PROVIDER ?? \"openai\";".into(),
                ),
                DiffLine::Added("  if (provider.trim() === \"\") {".into()),
                DiffLine::Added("    throw new Error(\"provider is required\");".into()),
                DiffLine::Added("  }".into()),
                DiffLine::Context("  return provider.trim();".into()),
                DiffLine::Context("}".into()),
            ],
        }];

        applier.apply_verified_hunks(&hunks).unwrap();

        let written = std::fs::read_to_string(tmp.path().join("src/config.js")).unwrap();
        assert_eq!(
            written.replace("\r\n", "\n"),
            "export function loadConfig(raw = {}, env = process.env) {\n  const provider = raw.provider ?? env.PROVIDER ?? \"openai\";\n  if (provider.trim() === \"\") {\n    throw new Error(\"provider is required\");\n  }\n  return provider.trim();\n}\n"
        );
    }

    #[test]
    fn apply_verified_hunks_rejects_context_when_model_omits_export() {
        let tmp = tempfile::tempdir().unwrap();
        let _repo = init_repo_with_seed(tmp.path());
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/config.js"),
            "export function loadConfig(raw = {}, env = process.env) {\n  const maxRetries = Number(raw.maxRetries || env.MAX_RETRIES || 2);\n  return maxRetries;\n}\n",
        )
        .unwrap();

        let mut applier = DiffApplier::open(tmp.path()).unwrap();
        let hunks = vec![DiffHunk {
            file_path: PathBuf::from("src/config.js"),
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 5,
            lines: vec![
                DiffLine::Context("function loadConfig(raw = {}, env = process.env) {".into()),
                DiffLine::Removed(
                    "  const maxRetries = Number(raw.maxRetries || env.MAX_RETRIES || 2);".into(),
                ),
                DiffLine::Added(
                    "  const maxRetriesRaw = raw.maxRetries ?? env.MAX_RETRIES ?? 2;".into(),
                ),
                DiffLine::Added("  const maxRetries = Number(maxRetriesRaw);".into()),
                DiffLine::Context("  return maxRetries;".into()),
                DiffLine::Context("}".into()),
            ],
        }];

        assert!(applier.apply_verified_hunks(&hunks).is_err());

        let written = std::fs::read_to_string(tmp.path().join("src/config.js")).unwrap();
        assert_eq!(
            written.replace("\r\n", "\n"),
            "export function loadConfig(raw = {}, env = process.env) {\n  const maxRetries = Number(raw.maxRetries || env.MAX_RETRIES || 2);\n  return maxRetries;\n}\n"
        );
    }
}
