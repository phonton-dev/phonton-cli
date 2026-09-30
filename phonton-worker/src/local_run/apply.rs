//! Explicit single-file application of a selected local candidate.
//! Generation never calls this module. The original Git index is observed but
//! never staged, reset, stashed or rewritten here.
#[cfg(windows)]
use super::validate_creation_recovery_path;
use super::{
    baseline_diff, baseline_diff_scoped, captured_hash, content_hash, integrity, invalid,
    inventory, inventory_excluding, js_writable_scope, persist_json, python_writable_scope,
    scoped_hash, validate_creation_path, Result,
};
use phonton_types::{
    local::CheckStatus,
    local_run::{
        CandidateEvidence, CandidateStage, CheckEvidence, CheckPurpose, GitIndexEvidence,
        LocalApplyCreate, LocalApplyFile, LocalApplyReceipt, LocalCheck, LocalFileIdentity,
        LocalRunReceipt,
    },
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    path::{Path, PathBuf},
};

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn passing_command(evidence: &CheckEvidence, check: &LocalCheck, purpose: CheckPurpose) -> bool {
    evidence.purpose == purpose
        && evidence.status == CheckStatus::Passed
        && evidence.exit_code == Some(0)
        && evidence
            .check
            .as_ref()
            .is_some_and(|actual| actual.program == check.program && actual.args == check.args)
}

fn reviewed_checks_match(receipt: &LocalRunReceipt, candidate: &CandidateEvidence) -> bool {
    if receipt.schema < 3 && js_writable_scope(&receipt.request) {
        return false;
    }
    if receipt.schema < 4 && python_writable_scope(&receipt.request) {
        return false;
    }
    let verification = &receipt.request.checks;
    if verification.is_empty() {
        return false;
    }
    let offset = usize::from(receipt.request.preparation.is_some());
    if candidate.checks.len() != verification.len() + offset {
        return false;
    }
    if let Some(preparation) = receipt.request.preparation.as_ref() {
        if !passing_command(&candidate.checks[0], preparation, CheckPurpose::Preparation) {
            return false;
        }
    }
    candidate.checks[offset..]
        .iter()
        .zip(verification)
        .all(|(evidence, check)| passing_command(evidence, check, CheckPurpose::Verification))
}

fn existing_regular_file(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(invalid(format!(
            "Apply evidence is not a regular file: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn replace_source(staged: tempfile::NamedTempFile, target: &Path) -> Result<()> {
    let temporary = staged.into_temp_path();
    replace_source_path(&temporary, target)
}

#[cfg(windows)]
fn replace_source_path(temporary: &Path, target: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "ReplaceFileW"]
        fn replace_file(
            replaced: *const u16,
            replacement: *const u16,
            backup: *const u16,
            flags: u32,
            exclude: *mut std::ffi::c_void,
            reserved: *mut std::ffi::c_void,
        ) -> i32;
    }
    let target_name: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let temporary_name: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    // Zero flags require the OS to preserve the replaced file's ACLs and attributes.
    let replaced = unsafe {
        replace_file(
            target_name.as_ptr(),
            temporary_name.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if replaced == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_source(staged: tempfile::NamedTempFile, target: &Path) -> Result<()> {
    staged.persist(target).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(not(windows))]
fn replace_source_path(temporary: &Path, target: &Path) -> Result<()> {
    std::fs::rename(temporary, target)?;
    Ok(())
}

/// Read a bounded, separate apply journal without changing the run receipt.
pub fn read_status(run_dir: &Path) -> Result<Option<LocalApplyReceipt>> {
    let path = run_dir.join("apply.json");
    if !existing_regular_file(&path)? {
        return Ok(None);
    }
    let metadata = std::fs::symlink_metadata(&path)?;
    if metadata.len() > 64 * 1024 {
        return Err(invalid("Apply journal exceeds read limit"));
    }
    Ok(Some(serde_json::from_slice(&std::fs::read(path)?)?))
}

fn save_status(run_dir: &Path, status: &LocalApplyReceipt) -> Result<()> {
    let path = run_dir.join("apply.json");
    existing_regular_file(&path)?;
    super::sync_directory(
        run_dir
            .parent()
            .ok_or_else(|| invalid("Apply run directory has no parent"))?,
    )?;
    persist_json(&path, &serde_json::to_value(status)?)
}

fn sync_source_parent(path: &Path) -> Result<()> {
    super::sync_directory(
        path.parent()
            .ok_or_else(|| invalid("Apply source has no parent directory"))?,
    )
}

fn sync_changed_parents(repository: &Path, changed: &[LocalApplyFile]) -> Result<()> {
    let mut parents = BTreeSet::new();
    for file in changed {
        let source = repository.join(&file.path);
        parents.insert(
            source
                .parent()
                .ok_or_else(|| invalid("Apply source has no parent directory"))?
                .to_path_buf(),
        );
    }
    for parent in parents {
        super::sync_directory(&parent)?;
    }
    Ok(())
}

fn write_new_evidence(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("Apply evidence has no parent directory"))?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    #[cfg(windows)]
    {
        let temporary = staged.into_temp_path();
        super::move_file_write_through(&temporary, path, false)?;
        super::sync_directory(parent)?;
    }
    #[cfg(not(windows))]
    {
        staged
            .persist_noclobber(path)
            .map_err(|error| error.error)?;
        super::sync_directory(parent)?;
    }
    Ok(())
}

/// Apply one existing file from a terminal, verified local receipt. Every
/// project write is preceded by source, candidate and index identity checks.
/// A synced prepared record and original-byte backup permit inspection if the
/// process stops between the atomic replacement and final acknowledgement.
pub async fn apply_one(
    receipt: &LocalRunReceipt,
    run_dir: &Path,
    candidate_number: u32,
    expected_candidate_sha256: &str,
) -> Result<LocalApplyReceipt> {
    apply_one_with_status_writer(
        receipt,
        run_dir,
        candidate_number,
        expected_candidate_sha256,
        save_status,
        sync_source_parent,
    )
    .await
}

async fn apply_one_with_status_writer(
    receipt: &LocalRunReceipt,
    run_dir: &Path,
    candidate_number: u32,
    expected_candidate_sha256: &str,
    write_status: fn(&Path, &LocalApplyReceipt) -> Result<()>,
    sync_source: fn(&Path) -> Result<()>,
) -> Result<LocalApplyReceipt> {
    if receipt.state != "review_ready" || receipt.selected_candidate != Some(candidate_number) {
        return Err(invalid(
            "Only the selected, verified candidate can be applied",
        ));
    }
    let candidate = receipt
        .candidates
        .iter()
        .find(|value| value.number == candidate_number)
        .ok_or_else(|| invalid("Selected candidate is missing"))?;
    if candidate.content_sha256.as_deref() != Some(expected_candidate_sha256)
        || candidate.stage != CandidateStage::Complete
        || !reviewed_checks_match(receipt, candidate)
    {
        return Err(invalid(
            "Reviewed candidate identity or verification is unavailable",
        ));
    }
    let index = receipt
        .git_index
        .as_ref()
        .filter(|value| value.status == CheckStatus::Passed && value.stage == "final review")
        .ok_or_else(|| invalid("Final Git index integrity was not proven"))?;
    let repository = std::fs::canonicalize(&receipt.request.repository)?;
    let run_dir = std::fs::canonicalize(run_dir)?;
    if run_dir.starts_with(&repository)
        || run_dir.file_name().and_then(|name| name.to_str()) != Some(receipt.id.as_str())
    {
        return Err(invalid(
            "Run evidence is not outside its repository under its saved ID",
        ));
    }
    let baseline = run_dir.join("baseline");
    let candidate_dir = run_dir.join(format!("candidate-{candidate_number}"));
    if std::fs::canonicalize(&candidate_dir)? != std::fs::canonicalize(&candidate.directory)? {
        return Err(invalid(
            "Selected candidate directory does not match saved run evidence",
        ));
    }
    let files = inventory(&repository).await?;
    if content_hash(&baseline, &files)? != receipt.baseline_sha256
        || content_hash(&candidate_dir, &files)? != expected_candidate_sha256
    {
        return Err(invalid(
            "Saved baseline or selected candidate bytes changed",
        ));
    }
    if baseline_diff(&baseline, &candidate_dir, &receipt.request.files)? != candidate.diff {
        return Err(invalid(
            "Selected diff no longer matches saved candidate bytes",
        ));
    }
    let mut changed = Vec::new();
    for path in &files {
        if std::fs::read(baseline.join(path))? != std::fs::read(candidate_dir.join(path))? {
            changed.push(path.clone());
        }
    }
    if changed.len() != 1 || !receipt.request.files.contains(&changed[0]) {
        return Err(invalid(
            "Guarded apply currently supports one changed existing source file",
        ));
    }
    let path = changed.remove(0);
    let before = std::fs::read(baseline.join(&path))?;
    let after = std::fs::read(candidate_dir.join(&path))?;
    let target = repository.join(&path);
    let _held_source_parents = hold_source_parents(&repository, std::iter::once(path.as_path()))?;
    let mut checked_index = index.clone();
    if !integrity::verify(&repository, &mut checked_index, "before apply").await {
        return Err(invalid("Original Git index changed since final review"));
    }
    let mut status = LocalApplyReceipt {
        schema: 1,
        run_id: receipt.id.clone(),
        candidate_number,
        path: Some(path.clone()),
        before_sha256: Some(hash(&before)),
        after_sha256: Some(hash(&after)),
        files: Vec::new(),
        created_file: None,
        baseline_sha256: None,
        candidate_sha256: None,
        state: "prepared".into(),
        rollback_temporaries: Vec::new(),
        detail: "Original bytes are backed up; source replacement is not yet confirmed.".into(),
    };
    let previous = read_status(&run_dir)?;
    let backup = run_dir.join("apply-backup.bin");
    if let Some(saved) = &previous {
        if saved.run_id != status.run_id
            || saved.candidate_number != status.candidate_number
            || saved.path != status.path
            || saved.before_sha256 != status.before_sha256
            || saved.after_sha256 != status.after_sha256
        {
            return Err(invalid(
                "Existing apply journal identifies different source bytes",
            ));
        }
        if !existing_regular_file(&backup)? {
            return Err(invalid(
                "Existing apply journal has no original-byte backup",
            ));
        }
        if std::fs::read(&backup)? != before {
            return Err(invalid("Apply backup does not match captured source"));
        }
        let current = std::fs::read(&target)?;
        if saved.state == "applied" || (saved.state == "prepared" && current == after) {
            if current != after {
                return Err(invalid(
                    "Previously applied source changed; no repeat write occurred",
                ));
            }
            sync_source(&target)?;
            status.state = "applied".into();
            status.detail = "Selected candidate bytes are present; the Git index still matches final review. No repeat write occurred.".into();
            write_status(&run_dir, &status)?;
            return Ok(status);
        }
        if saved.state != "prepared" || current != before {
            return Err(invalid(
                "Unfinished apply needs manual review; source differs from both saved states",
            ));
        }
    }
    if captured_hash(&repository, &files)? != receipt.baseline_sha256 {
        return Err(invalid("Repository source changed since final review"));
    }
    if existing_regular_file(&backup)? {
        if std::fs::read(&backup)? != before {
            return Err(invalid("Apply backup does not match captured source"));
        }
    } else {
        write_new_evidence(&backup, &before)?;
    }
    write_status(&run_dir, &status)?;
    let source_metadata = std::fs::symlink_metadata(&target)?;
    if !source_metadata.file_type().is_file() {
        return Err(invalid("Apply source is not a regular file"));
    }
    let parent = target
        .parent()
        .ok_or_else(|| invalid("Apply source has no parent directory"))?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged.write_all(&after)?;
    #[cfg(not(windows))]
    staged
        .as_file()
        .set_permissions(source_metadata.permissions())?;
    staged.as_file().sync_all()?;
    if captured_hash(&repository, &files)? != receipt.baseline_sha256
        || !integrity::verify(&repository, &mut checked_index, "immediately before apply").await
    {
        return Err(invalid(
            "Source or Git index changed before replacement; no project file was replaced",
        ));
    }
    replace_source(staged, &target)?;
    if std::fs::read(&target)? != after
        || !integrity::verify(&repository, &mut checked_index, "after apply").await
    {
        return Err(invalid("Apply needs recovery: source or Git index changed after replacement; backup and prepared journal were retained"));
    }
    sync_source(&target)?;
    status.state = "applied".into();
    status.detail = "One verified source file was replaced; the original Git index was unchanged. Original bytes remain in the run backup.".into();
    write_status(&run_dir, &status)?;
    Ok(status)
}

fn batch_file(
    path: PathBuf,
    before: &[u8],
    after: &[u8],
    run_id: &str,
    ordinal: usize,
) -> LocalApplyFile {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let temporary = parent.join(format!(
        ".phonton-apply-{}-{ordinal}.tmp",
        hash(run_id.as_bytes())
    ));
    LocalApplyFile {
        path,
        before_sha256: hash(before),
        after_sha256: hash(after),
        backup: PathBuf::from(format!("apply-backup-{ordinal}.bin")),
        temporary,
    }
}

fn verify_recorded_temporary(repository: &Path, file: &LocalApplyFile) -> Result<bool> {
    let target = repository.join(&file.temporary);
    let source = repository.join(&file.path);
    if target.parent() != source.parent() {
        return Err(invalid("Apply temporary is not beside its source"));
    }
    if !existing_regular_file(&target)? {
        return Ok(false);
    }
    if hash(&std::fs::read(&target)?) != file.after_sha256 {
        return Err(invalid(format!(
            "Recorded apply temporary has unexpected bytes: {}",
            file.temporary.display()
        )));
    }
    Ok(true)
}

async fn checked_batch_inventory(
    repository: &Path,
    changed: &[LocalApplyFile],
) -> Result<Vec<PathBuf>> {
    let mut excluded = BTreeSet::new();
    for file in changed {
        verify_recorded_temporary(repository, file)?;
        excluded.insert(file.temporary.clone());
    }
    inventory_excluding(repository, &excluded).await
}

fn classify_sources(
    repository: &Path,
    baseline: &Path,
    candidate_dir: &Path,
    files: &[PathBuf],
    changed: &[LocalApplyFile],
) -> Result<Vec<bool>> {
    let mut applied = Vec::with_capacity(changed.len());
    for path in files {
        let target = repository.join(path);
        if !existing_regular_file(&target)? {
            return Err(invalid(format!(
                "Apply source is missing: {}",
                path.display()
            )));
        }
        let current = std::fs::read(&target)?;
        let before = std::fs::read(baseline.join(path))?;
        if let Some(file) = changed.iter().find(|file| &file.path == path) {
            let after = std::fs::read(candidate_dir.join(path))?;
            if current == after {
                applied.push(true);
            } else if current == before {
                applied.push(false);
            } else {
                return Err(invalid(format!(
                    "Apply needs recovery: source differs from both saved states: {}",
                    file.path.display()
                )));
            }
        } else if current != before {
            return Err(invalid(format!(
                "Unchanged source moved since review: {}",
                path.display()
            )));
        }
    }
    Ok(applied)
}

/// Apply the complete selected changed-file set. A schema-2 prepared journal
/// permits explicit roll-forward after interruption; no multi-file atomicity is
/// claimed. Existing schema-1 one-file journals retain their original behavior.
pub async fn apply_selected(
    receipt: &LocalRunReceipt,
    run_dir: &Path,
    candidate_number: u32,
    expected_candidate_sha256: &str,
) -> Result<LocalApplyReceipt> {
    let previous = read_status(run_dir)?;
    if let Some(path) = &receipt.request.new_file {
        if !receipt.request.editable_existing.is_empty() {
            return apply_mixed(
                receipt,
                run_dir,
                candidate_number,
                expected_candidate_sha256,
                path,
                previous,
            )
            .await;
        }
        return apply_creation(
            receipt,
            run_dir,
            candidate_number,
            expected_candidate_sha256,
            path,
            previous,
        )
        .await;
    }
    if previous.as_ref().is_some_and(|status| status.schema == 1) {
        return apply_one(
            receipt,
            run_dir,
            candidate_number,
            expected_candidate_sha256,
        )
        .await;
    }
    if previous.as_ref().is_some_and(|status| status.schema != 2) {
        return Err(invalid("Unknown apply journal schema"));
    }
    if receipt.state != "review_ready" || receipt.selected_candidate != Some(candidate_number) {
        return Err(invalid(
            "Only the selected, verified candidate can be applied",
        ));
    }
    let candidate = receipt
        .candidates
        .iter()
        .find(|value| value.number == candidate_number)
        .ok_or_else(|| invalid("Selected candidate is missing"))?;
    if candidate.content_sha256.as_deref() != Some(expected_candidate_sha256)
        || candidate.stage != CandidateStage::Complete
        || !reviewed_checks_match(receipt, candidate)
    {
        return Err(invalid(
            "Reviewed candidate identity or verification is unavailable",
        ));
    }
    let index: GitIndexEvidence = receipt
        .git_index
        .as_ref()
        .filter(|value| value.status == CheckStatus::Passed && value.stage == "final review")
        .ok_or_else(|| invalid("Final Git index integrity was not proven"))?
        .clone();
    let repository = std::fs::canonicalize(&receipt.request.repository)?;
    let run_dir = std::fs::canonicalize(run_dir)?;
    if run_dir.starts_with(&repository)
        || run_dir.file_name().and_then(|name| name.to_str()) != Some(receipt.id.as_str())
    {
        return Err(invalid(
            "Run evidence is not outside its repository under its saved ID",
        ));
    }
    let baseline = run_dir.join("baseline");
    let candidate_dir = run_dir.join(format!("candidate-{candidate_number}"));
    if std::fs::canonicalize(&candidate_dir)? != std::fs::canonicalize(&candidate.directory)? {
        return Err(invalid(
            "Selected candidate directory does not match saved run evidence",
        ));
    }

    let mut excluded = BTreeSet::new();
    if let Some(saved) = &previous {
        if saved.run_id != receipt.id
            || saved.candidate_number != candidate_number
            || saved.baseline_sha256.as_deref() != Some(receipt.baseline_sha256.as_str())
            || saved.candidate_sha256.as_deref() != Some(expected_candidate_sha256)
            || !matches!(saved.state.as_str(), "prepared" | "applied")
            || saved.files.is_empty()
            || saved.files.len() > receipt.request.files.len()
        {
            return Err(invalid(
                "Existing apply journal identifies different candidate evidence",
            ));
        }
        for (ordinal, file) in saved.files.iter().enumerate() {
            phonton_local::edit::safe_relative_path(&file.path.to_string_lossy())?;
            let expected = batch_file(file.path.clone(), &[], &[], &receipt.id, ordinal);
            if file.backup != expected.backup
                || file.temporary != expected.temporary
                || !receipt.request.files.contains(&file.path)
            {
                return Err(invalid(
                    "Existing apply journal has an invalid source or temporary path",
                ));
            }
            verify_recorded_temporary(&repository, file)?;
            excluded.insert(file.temporary.clone());
        }
    }
    let files = inventory_excluding(&repository, &excluded).await?;
    if content_hash(&baseline, &files)? != receipt.baseline_sha256
        || content_hash(&candidate_dir, &files)? != expected_candidate_sha256
        || baseline_diff(&baseline, &candidate_dir, &receipt.request.files)? != candidate.diff
    {
        return Err(invalid("Saved baseline, candidate, or diff changed"));
    }
    let mut changed = Vec::new();
    for path in &files {
        let before = std::fs::read(baseline.join(path))?;
        let after = std::fs::read(candidate_dir.join(path))?;
        if before != after {
            if !receipt.request.files.contains(path) {
                return Err(invalid(
                    "Candidate changed a file outside the reviewed scope",
                ));
            }
            changed.push(batch_file(
                path.clone(),
                &before,
                &after,
                &receipt.id,
                changed.len(),
            ));
        }
    }
    if changed.is_empty() || changed.len() > receipt.request.files.len() {
        return Err(invalid("Selected candidate has no bounded source changes"));
    }
    let _held_source_parents =
        hold_source_parents(&repository, changed.iter().map(|file| file.path.as_path()))?;
    let mut status = LocalApplyReceipt {
        schema: 2,
        run_id: receipt.id.clone(),
        candidate_number,
        path: None,
        before_sha256: None,
        after_sha256: None,
        files: changed.clone(),
        created_file: None,
        baseline_sha256: Some(receipt.baseline_sha256.clone()),
        candidate_sha256: Some(expected_candidate_sha256.to_owned()),
        state: "prepared".into(),
        rollback_temporaries: Vec::new(),
        detail: "All original bytes are backed up; some replacements may need recovery.".into(),
    };
    if let Some(saved) = &previous {
        if saved.files != changed {
            return Err(invalid(
                "Existing apply journal identifies different source bytes",
            ));
        }
    } else {
        if captured_hash(&repository, &files)? != receipt.baseline_sha256 {
            return Err(invalid("Repository source changed since final review"));
        }
        for file in &changed {
            if existing_regular_file(&repository.join(&file.temporary))? {
                return Err(invalid("Unrecorded apply temporary already exists"));
            }
        }
    }
    let mut checked_index = index;
    if !integrity::verify(&repository, &mut checked_index, "before apply").await {
        return Err(invalid("Original Git index changed since final review"));
    }
    for file in &changed {
        let backup = run_dir.join(&file.backup);
        let before = std::fs::read(baseline.join(&file.path))?;
        if existing_regular_file(&backup)? {
            if std::fs::read(&backup)? != before {
                return Err(invalid("Apply backup does not match captured source"));
            }
        } else if previous.is_some() {
            return Err(invalid("Prepared apply is missing an original-byte backup"));
        } else {
            write_new_evidence(&backup, &before)?;
        }
    }
    if previous.is_none() {
        save_status(&run_dir, &status)?;
    }
    let states = classify_sources(&repository, &baseline, &candidate_dir, &files, &changed)?;
    if previous
        .as_ref()
        .is_some_and(|saved| saved.state == "applied")
    {
        if states.iter().all(|state| *state) {
            for file in &changed {
                if verify_recorded_temporary(&repository, file)? {
                    std::fs::remove_file(repository.join(&file.temporary))?;
                }
            }
            sync_changed_parents(&repository, &changed)?;
            if let Some(saved) = previous {
                return Ok(saved);
            }
        }
        return Err(invalid(
            "Previously applied source changed; no repeat write occurred",
        ));
    }
    for (file, already_applied) in changed.iter().zip(states) {
        if already_applied {
            continue;
        }
        if checked_batch_inventory(&repository, &changed).await? != files
            || !integrity::verify(&repository, &mut checked_index, "immediately before apply").await
            || classify_sources(&repository, &baseline, &candidate_dir, &files, &changed).is_err()
        {
            return Err(invalid(
                "Apply needs recovery: source or Git index changed before replacement",
            ));
        }
        let target = repository.join(&file.path);
        let current = std::fs::read(&target)?;
        if current == std::fs::read(candidate_dir.join(&file.path))? {
            continue;
        }
        if current != std::fs::read(baseline.join(&file.path))? {
            return Err(invalid(
                "Apply needs recovery: target changed before replacement",
            ));
        }
        let temporary = repository.join(&file.temporary);
        // A recovered temp may be a hard link to another file with identical
        // bytes. Unlink it and create a fresh exclusive file before any chmod
        // or platform replacement; never operate on its shared inode.
        if verify_recorded_temporary(&repository, file)? {
            std::fs::remove_file(&temporary)?;
        }
        let after = std::fs::read(candidate_dir.join(&file.path))?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        output.write_all(&after)?;
        #[cfg(not(windows))]
        output.set_permissions(std::fs::symlink_metadata(&target)?.permissions())?;
        output.sync_all()?;
        drop(output);
        if !verify_recorded_temporary(&repository, file)?
            || checked_batch_inventory(&repository, &changed).await? != files
            || std::fs::read(&target)? != std::fs::read(baseline.join(&file.path))?
            || !integrity::verify(&repository, &mut checked_index, "before replacement").await
        {
            return Err(invalid(
                "Apply needs recovery: source, temporary or Git index changed before replacement",
            ));
        }
        replace_source_path(&temporary, &target)?;
        if std::fs::read(&target)? != std::fs::read(candidate_dir.join(&file.path))?
            || !integrity::verify(&repository, &mut checked_index, "after apply replacement").await
        {
            return Err(invalid(
                "Apply needs recovery: source or Git index changed after replacement",
            ));
        }
    }
    if checked_batch_inventory(&repository, &changed).await? != files
        || content_hash(&baseline, &files)? != receipt.baseline_sha256
        || content_hash(&candidate_dir, &files)? != expected_candidate_sha256
        || !classify_sources(&repository, &baseline, &candidate_dir, &files, &changed)?
            .iter()
            .all(|state| *state)
        || !integrity::verify(&repository, &mut checked_index, "after apply").await
    {
        return Err(invalid(
            "Apply needs recovery: the selected batch is not fully present",
        ));
    }
    for file in &changed {
        if verify_recorded_temporary(&repository, file)? {
            std::fs::remove_file(repository.join(&file.temporary))?;
        }
    }
    sync_changed_parents(&repository, &changed)?;
    status.state = "applied".into();
    status.detail = format!("{} verified source files match the selected candidate; original bytes remain in per-file backups. The Git index was unchanged.", changed.len());
    save_status(&run_dir, &status)?;
    Ok(status)
}

fn creation_entry(path: &Path, after: &[u8], run_id: &str) -> LocalApplyCreate {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    LocalApplyCreate {
        path: path.to_path_buf(),
        after_sha256: hash(after),
        temporary: parent.join(format!(".phonton-create-{}.tmp", hash(run_id.as_bytes()))),
        anchor: Some(PathBuf::from("apply-created-anchor.bin")),
        identity: None,
        publication_attempted: false,
        rollback_deletion_attempted: false,
    }
}

fn directory_device(path: &Path) -> Result<u64> {
    #[cfg(windows)]
    {
        let held = hold_apply_parent(path)?;
        let directory = held
            .last()
            .ok_or_else(|| invalid("Apply directory has no held handle"))?;
        Ok(creation_identity(directory)?.device)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(std::fs::metadata(path)?.dev())
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = path;
        Err(invalid("Creation identity is unavailable on this platform"))
    }
}

fn git_creation_anchor(repository: &Path, run_dir: &Path) -> Result<PathBuf> {
    let git_dir = repository.join(".git");
    let metadata = std::fs::symlink_metadata(&git_dir)?;
    if !metadata.file_type().is_dir() {
        return Err(invalid(
            "Creation needs a real local .git directory for its same-volume anchor",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(invalid("Creation Git directory is a reparse point"));
        }
    }
    let run_id = run_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("Creation run directory has no ID"))?;
    Ok(git_dir.join(format!(".phonton-created-{}.bin", hash(run_id.as_bytes()))))
}

fn creation_anchor_path(
    repository: &Path,
    run_dir: &Path,
    entry: &LocalApplyCreate,
) -> Result<PathBuf> {
    match entry.anchor.as_deref() {
        Some(anchor) if anchor == Path::new("apply-created-anchor.bin") => Ok(run_dir.join(anchor)),
        Some(anchor) if anchor == git_creation_anchor(repository, run_dir)?.as_path() => {
            Ok(anchor.to_path_buf())
        }
        _ => Err(invalid("Creation journal has no trusted identity anchor")),
    }
}

fn select_creation_anchor(
    repository: &Path,
    run_dir: &Path,
    target_parent: &Path,
) -> Result<PathBuf> {
    let device = directory_device(target_parent)?;
    if directory_device(run_dir)? == device {
        return Ok(PathBuf::from("apply-created-anchor.bin"));
    }
    let git_dir = repository.join(".git");
    let anchor = git_creation_anchor(repository, run_dir).map_err(|_| {
        invalid("Creation needs same-volume run evidence or an ordinary local .git directory")
    })?;
    if directory_device(&git_dir)? != device {
        return Err(invalid(
            "Creation cannot retain a same-volume identity anchor for this repository",
        ));
    }
    Ok(anchor)
}

fn creation_identity(file: &std::fs::File) -> Result<LocalFileIdentity> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        #[repr(C)]
        struct FileIdInfo {
            volume_serial_number: u64,
            file_id: [u8; 16],
        }
        #[link(name = "kernel32")]
        extern "system" {
            #[link_name = "GetFileInformationByHandleEx"]
            fn get_file_information_by_handle_ex(
                file: *mut std::ffi::c_void,
                info_class: u32,
                info: *mut std::ffi::c_void,
                size: u32,
            ) -> i32;
        }
        // FILE_INFO_BY_HANDLE_CLASS::FileIdInfo is 18. The 128-bit ID avoids
        // the truncated 64-bit ID that may collide on ReFS.
        let mut info = FileIdInfo {
            volume_serial_number: 0,
            file_id: [0; 16],
        };
        let result = unsafe {
            get_file_information_by_handle_ex(
                file.as_raw_handle(),
                18,
                (&mut info as *mut FileIdInfo).cast(),
                std::mem::size_of::<FileIdInfo>() as u32,
            )
        };
        if result == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(LocalFileIdentity {
            kind: "windows_file_id_128".into(),
            device: info.volume_serial_number,
            file_id: info
                .file_id
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        })
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(LocalFileIdentity {
            kind: "unix_dev_inode".into(),
            device: metadata.dev(),
            file_id: format!("{:016x}", metadata.ino()),
        })
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = file;
        Err(invalid("Creation identity is unavailable on this platform"))
    }
}

fn observe_creation_handle(
    mut file: std::fs::File,
) -> Result<(std::fs::File, LocalFileIdentity, String)> {
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(invalid("Created-file identity is a reparse point"));
        }
    }
    if !metadata.file_type().is_file() || metadata.len() > 1024 * 1024 {
        return Err(invalid(
            "Created-file identity is not a bounded regular file",
        ));
    }
    let identity = creation_identity(&file)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)?;
    if bytes.len() as u64 != metadata.len() {
        return Err(invalid(
            "Created-file bytes changed during identity observation",
        ));
    }
    Ok((file, identity, hash(&bytes)))
}

fn open_observed_creation_file(path: &Path) -> Result<(std::fs::File, LocalFileIdentity, String)> {
    if !existing_regular_file(path)? {
        return Err(invalid("Created-file identity is missing or not regular"));
    }
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?
    };
    #[cfg(not(windows))]
    let file = std::fs::File::open(path)?;
    observe_creation_handle(file)
}

fn observe_creation_file(path: &Path) -> Result<(LocalFileIdentity, String)> {
    let (_, identity, after_sha256) = open_observed_creation_file(path)?;
    Ok((identity, after_sha256))
}

#[cfg(windows)]
fn open_created_deletion_handles(
    target: &Path,
    anchor: &Path,
    entry: &LocalApplyCreate,
) -> Result<(std::fs::File, std::fs::File)> {
    use std::os::windows::fs::OpenOptionsExt;
    const DELETE: u32 = 0x0001_0000;
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let expected = entry
        .identity
        .as_ref()
        .ok_or_else(|| invalid("Creation journal has no published file identity"))?;
    // The target handle denies both write and delete sharing. Another process
    // cannot replace this name between our identity check and disposition.
    let target_file = std::fs::OpenOptions::new()
        .access_mode(GENERIC_READ | DELETE)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(target)?;
    let (target_file, target_identity, target_hash) = observe_creation_handle(target_file)?;
    if &target_identity != expected || target_hash != entry.after_sha256 {
        return Err(invalid("Created target is not the published file"));
    }
    // This handle must share DELETE with the target handle, but its live file
    // identity still proves the target is linked to the retained witness.
    let anchor_file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(anchor)?;
    let (anchor_file, anchor_identity, anchor_hash) = observe_creation_handle(anchor_file)?;
    if &anchor_identity != expected || anchor_hash != entry.after_sha256 {
        return Err(invalid("Creation anchor changed before rollback"));
    }
    Ok((target_file, anchor_file))
}

#[cfg(windows)]
fn dispose_created_target(target_file: std::fs::File) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct FileDispositionInfo {
        delete_file: u8,
    }
    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "SetFileInformationByHandle"]
        fn set_file_information_by_handle(
            file: *mut std::ffi::c_void,
            info_class: u32,
            info: *const std::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    let info = FileDispositionInfo { delete_file: 1 };
    // FILE_INFO_BY_HANDLE_CLASS::FileDispositionInfo is 4. The filesystem
    // removes the validated link when this DELETE-capable handle closes.
    let result = unsafe {
        set_file_information_by_handle(
            target_file.as_raw_handle(),
            4,
            (&info as *const FileDispositionInfo).cast(),
            std::mem::size_of::<FileDispositionInfo>() as u32,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    drop(target_file);
    Ok(())
}

fn hold_created_anchor(
    repository: &Path,
    run_dir: &Path,
    entry: &LocalApplyCreate,
) -> Result<std::fs::File> {
    let anchor = creation_anchor_path(repository, run_dir, entry)?;
    let (file, identity, after_sha256) = open_observed_creation_file(&anchor)?;
    if Some(&identity) != entry.identity.as_ref() || after_sha256 != entry.after_sha256 {
        return Err(invalid("Creation anchor changed before publication"));
    }
    Ok(file)
}

fn verify_created_ownership(
    repository: &Path,
    run_dir: &Path,
    entry: &LocalApplyCreate,
    target_present: bool,
) -> Result<()> {
    let anchor = creation_anchor_path(repository, run_dir, entry)?;
    let identity = entry
        .identity
        .as_ref()
        .ok_or_else(|| invalid("Creation journal has no published file identity"))?;
    let (anchor_identity, anchor_hash) = observe_creation_file(&anchor)?;
    if &anchor_identity != identity || anchor_hash != entry.after_sha256 {
        return Err(invalid(
            "Creation anchor changed; manual recovery is required",
        ));
    }
    if target_present {
        let (target_identity, target_hash) = observe_creation_file(&repository.join(&entry.path))?;
        if &target_identity != identity || target_hash != entry.after_sha256 {
            return Err(invalid(
                "Created target is not the published file; manual recovery is required",
            ));
        }
    }
    Ok(())
}

fn verify_creation_temporary(repository: &Path, entry: &LocalApplyCreate) -> Result<bool> {
    let temporary = repository.join(&entry.temporary);
    if temporary.parent() != repository.join(&entry.path).parent() {
        return Err(invalid("Creation temporary is not beside its target"));
    }
    if !existing_regular_file(&temporary)? {
        return Ok(false);
    }
    if std::fs::metadata(&temporary)?.len() > 1024 * 1024
        || hash(&std::fs::read(&temporary)?) != entry.after_sha256
    {
        return Err(invalid("Recorded creation temporary has unexpected bytes"));
    }
    if let Some(identity) = &entry.identity {
        let (observed, after_sha256) = observe_creation_file(&temporary)?;
        if &observed != identity || after_sha256 != entry.after_sha256 {
            return Err(invalid("Creation temporary is not the witnessed file"));
        }
    }
    Ok(true)
}

fn classify_creation_target(
    repository: &Path,
    run_dir: &Path,
    entry: &LocalApplyCreate,
) -> Result<bool> {
    let target = repository.join(&entry.path);
    if !existing_regular_file(&target)? {
        return Ok(false);
    }
    verify_created_ownership(repository, run_dir, entry, true)?;
    Ok(true)
}

async fn verify_unpublished_mixed_target(
    repository: &Path,
    run_dir: &Path,
    entry: &LocalApplyCreate,
    path: &Path,
) -> Result<()> {
    if classify_creation_target(repository, run_dir, entry)? {
        return Err(invalid(
            "Mixed apply needs recovery: creation target was already published",
        ));
    }
    validate_creation_path(repository, path, false)
        .await
        .map_err(|error| {
            invalid(format!(
                "Mixed apply needs recovery: creation path recheck failed: {error}"
            ))
        })
}

fn creation_entry_matches(actual: &LocalApplyCreate, expected: &LocalApplyCreate) -> bool {
    actual.path == expected.path
        && actual.after_sha256 == expected.after_sha256
        && actual.temporary == expected.temporary
        && actual.anchor == expected.anchor
}

fn stage_creation(
    repository: &Path,
    run_dir: &Path,
    after: &[u8],
    status: &mut LocalApplyReceipt,
) -> Result<LocalApplyCreate> {
    let mut entry = status
        .created_file
        .clone()
        .ok_or_else(|| invalid("Creation journal has no target"))?;
    let temporary = repository.join(&entry.temporary);
    let anchor = creation_anchor_path(repository, run_dir, &entry)?;
    if entry.identity.is_some() {
        verify_created_ownership(repository, run_dir, &entry, false)?;
        if verify_creation_temporary(repository, &entry)? {
            let (identity, after_sha256) = observe_creation_file(&temporary)?;
            if Some(&identity) != entry.identity.as_ref() || after_sha256 != entry.after_sha256 {
                return Err(invalid("Creation temporary is not the witnessed file"));
            }
        } else {
            std::fs::hard_link(&anchor, &temporary)?;
        }
        return Ok(entry);
    }
    if entry.publication_attempted
        || existing_regular_file(&anchor)?
        || verify_creation_temporary(repository, &entry)?
    {
        return Err(invalid(
            "Unwitnessed creation temporary or anchor needs manual inspection",
        ));
    }
    write_new_evidence(&anchor, after)?;
    let (identity, after_sha256) = observe_creation_file(&anchor)?;
    if after_sha256 != entry.after_sha256 {
        return Err(invalid(
            "Staged creation differs from the selected candidate",
        ));
    }
    entry.identity = Some(identity);
    verify_created_ownership(repository, run_dir, &entry, false)?;
    status.created_file = Some(entry.clone());
    save_status(run_dir, status)?;
    std::fs::hard_link(&anchor, &temporary).map_err(|error| {
        invalid(format!(
            "Cannot stage creation from same-volume identity anchor {}: {error}",
            anchor.display()
        ))
    })?;
    Ok(entry)
}

fn mark_creation_publication_attempted(
    run_dir: &Path,
    status: &mut LocalApplyReceipt,
) -> Result<()> {
    let entry = status
        .created_file
        .as_mut()
        .ok_or_else(|| invalid("Creation journal has no target"))?;
    if !entry.publication_attempted {
        entry.publication_attempted = true;
        save_status(run_dir, status)?;
    }
    Ok(())
}

#[cfg(windows)]
fn hold_apply_parent(parent: &Path) -> Result<Vec<std::fs::File>> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    // Hold every ancestor from the volume root through an Apply target parent.
    // Denying delete sharing prevents a path component from being renamed or
    // replaced while path-based publication or source replacement is in flight.
    // Write sharing remains necessary to create the temporary and final child.
    let mut held = Vec::new();
    for directory in parent
        .ancestors()
        .filter(|path| path.is_absolute())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(directory)?;
        let metadata = handle.metadata()?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(invalid("Apply parent contains a reparse point"));
        }
        held.push(handle);
    }
    Ok(held)
}

#[cfg(not(windows))]
fn hold_apply_parent(_parent: &Path) -> Result<Vec<std::fs::File>> {
    Ok(Vec::new())
}

fn hold_source_parents<'a>(
    repository: &Path,
    paths: impl IntoIterator<Item = &'a Path>,
) -> Result<Vec<std::fs::File>> {
    let mut parents = BTreeSet::new();
    for path in paths {
        let target = repository.join(path);
        parents.insert(
            target
                .parent()
                .ok_or_else(|| invalid("Apply source has no parent directory"))?
                .to_path_buf(),
        );
    }
    let mut held = Vec::new();
    for parent in parents {
        held.extend(hold_apply_parent(&parent)?);
    }
    Ok(held)
}

async fn checked_mixed_inventory(
    repository: &Path,
    changed: &[LocalApplyFile],
    created: &LocalApplyCreate,
) -> Result<Vec<PathBuf>> {
    let mut excluded = BTreeSet::from([created.path.clone(), created.temporary.clone()]);
    verify_creation_temporary(repository, created)?;
    for file in changed {
        verify_recorded_temporary(repository, file)?;
        excluded.insert(file.temporary.clone());
    }
    inventory_excluding(repository, &excluded).await
}

/// Roll forward one verified new file and its reviewed existing edits. The
/// schema-4 journal records every path before mutation; the multi-file result
/// is recoverable but cannot be claimed atomic across project files.
async fn apply_mixed(
    receipt: &LocalRunReceipt,
    run_dir: &Path,
    candidate_number: u32,
    expected_candidate_sha256: &str,
    path: &Path,
    previous: Option<LocalApplyReceipt>,
) -> Result<LocalApplyReceipt> {
    if receipt.state != "review_ready" || receipt.selected_candidate != Some(candidate_number) {
        return Err(invalid(
            "Only the selected, verified candidate can be applied",
        ));
    }
    let candidate = receipt
        .candidates
        .iter()
        .find(|value| value.number == candidate_number)
        .ok_or_else(|| invalid("Selected candidate is missing"))?;
    if candidate.stage != CandidateStage::Complete
        || candidate.content_sha256.as_deref() != Some(expected_candidate_sha256)
        || !reviewed_checks_match(receipt, candidate)
    {
        return Err(invalid(
            "Reviewed candidate identity or verification is unavailable",
        ));
    }
    let mut index = receipt
        .git_index
        .as_ref()
        .filter(|value| value.status == CheckStatus::Passed && value.stage == "final review")
        .ok_or_else(|| invalid("Final Git index integrity was not proven"))?
        .clone();
    let repository = std::fs::canonicalize(&receipt.request.repository)?;
    let run_dir = std::fs::canonicalize(run_dir)?;
    if run_dir.starts_with(&repository)
        || run_dir.file_name().and_then(|name| name.to_str()) != Some(receipt.id.as_str())
    {
        return Err(invalid(
            "Run evidence is not outside its repository under its saved ID",
        ));
    }
    let baseline = run_dir.join("baseline");
    let candidate_dir = run_dir.join(format!("candidate-{candidate_number}"));
    if std::fs::canonicalize(&candidate_dir)? != std::fs::canonicalize(&candidate.directory)? {
        return Err(invalid(
            "Selected candidate directory does not match saved run evidence",
        ));
    }
    let target = repository.join(path);
    let parent = target
        .parent()
        .ok_or_else(|| invalid("Creation target has no parent directory"))?;
    let _held_parent = hold_apply_parent(parent)?;
    validate_creation_path(&repository, path, previous.is_some()).await?;
    let new_bytes = std::fs::read(candidate_dir.join(path))?;
    phonton_local::edit::canonical_new_file(
        path,
        std::str::from_utf8(&new_bytes).map_err(|_| invalid("Created file is not UTF-8 text"))?,
    )?;
    let mut expected_created = creation_entry(path, &new_bytes, &receipt.id);
    expected_created.anchor = Some(select_creation_anchor(&repository, &run_dir, parent)?);
    let expected_anchor = creation_anchor_path(&repository, &run_dir, &expected_created)?;
    let _held_anchor_parent = hold_apply_parent(
        expected_anchor
            .parent()
            .ok_or_else(|| invalid("Creation anchor has no parent directory"))?,
    )?;
    if let Some(saved) = &previous {
        if saved.schema != 4
            || saved.run_id != receipt.id
            || saved.candidate_number != candidate_number
            || !saved
                .created_file
                .as_ref()
                .is_some_and(|entry| creation_entry_matches(entry, &expected_created))
            || saved.baseline_sha256.as_deref() != Some(receipt.baseline_sha256.as_str())
            || saved.candidate_sha256.as_deref() != Some(expected_candidate_sha256)
            || !matches!(saved.state.as_str(), "prepared" | "applied")
            || saved.files.is_empty()
            || saved.files.len() > receipt.request.editable_existing.len()
        {
            return Err(invalid(
                "Existing mixed apply journal identifies different candidate evidence",
            ));
        }
        for (ordinal, file) in saved.files.iter().enumerate() {
            phonton_local::edit::safe_relative_path(&file.path.to_string_lossy())?;
            let expected = batch_file(file.path.clone(), &[], &[], &receipt.id, ordinal);
            if file.backup != expected.backup
                || file.temporary != expected.temporary
                || !receipt.request.editable_existing.contains(&file.path)
            {
                return Err(invalid(
                    "Existing mixed apply journal has an invalid source or temporary path",
                ));
            }
        }
    }
    let created = previous
        .as_ref()
        .and_then(|saved| saved.created_file.clone())
        .unwrap_or(expected_created);
    let target_applied = classify_creation_target(&repository, &run_dir, &created)?;
    let creation_temp_exists = verify_creation_temporary(&repository, &created)?;
    if previous.is_none() && (target_applied || creation_temp_exists) {
        return Err(invalid(
            "Creation target or unrecorded temporary already exists",
        ));
    }
    if previous.is_some()
        && ((target_applied && !created.publication_attempted)
            || (!target_applied && created.publication_attempted)
            || (created.identity.is_none() && creation_temp_exists))
    {
        return Err(invalid(
            "Creation publication or temporary lacks ownership proof; manual recovery is required",
        ));
    }
    let mut excluded = BTreeSet::from([created.path.clone(), created.temporary.clone()]);
    if let Some(saved) = &previous {
        for file in &saved.files {
            verify_recorded_temporary(&repository, file)?;
            excluded.insert(file.temporary.clone());
        }
    }
    let files = inventory_excluding(&repository, &excluded).await?;
    if scoped_hash(&baseline, &files, Some(&created.path), true)? != receipt.baseline_sha256
        || scoped_hash(&candidate_dir, &files, Some(&created.path), true)?
            != expected_candidate_sha256
        || baseline_diff_scoped(
            &baseline,
            &candidate_dir,
            &receipt.request.files,
            Some(&created.path),
        )? != candidate.diff
    {
        return Err(invalid("Saved baseline, mixed candidate, or diff changed"));
    }
    let mut changed = Vec::new();
    for source in &files {
        let before = std::fs::read(baseline.join(source))?;
        let after = std::fs::read(candidate_dir.join(source))?;
        if before != after {
            if !receipt.request.editable_existing.contains(source) {
                return Err(invalid(format!(
                    "Mixed candidate changed source outside its reviewed edit scope: {}",
                    source.display()
                )));
            }
            changed.push(batch_file(
                source.clone(),
                &before,
                &after,
                &receipt.id,
                changed.len(),
            ));
        }
    }
    if changed.is_empty() || changed.len() > 3 {
        return Err(invalid(
            "Mixed candidate has no bounded existing source edit",
        ));
    }
    let _held_source_parents =
        hold_source_parents(&repository, changed.iter().map(|file| file.path.as_path()))?;
    if let Some(saved) = &previous {
        if saved.files != changed {
            return Err(invalid(
                "Existing mixed apply journal identifies different source bytes",
            ));
        }
    } else {
        if captured_hash(&repository, &files)? != captured_hash(&baseline, &files)? {
            return Err(invalid("Repository source changed since final review"));
        }
        for file in &changed {
            if existing_regular_file(&repository.join(&file.temporary))? {
                return Err(invalid("Unrecorded apply temporary already exists"));
            }
        }
    }
    if !integrity::verify(&repository, &mut index, "before mixed apply").await {
        return Err(invalid("Original Git index changed since final review"));
    }
    for file in &changed {
        let backup = run_dir.join(&file.backup);
        let before = std::fs::read(baseline.join(&file.path))?;
        if existing_regular_file(&backup)? {
            if std::fs::read(&backup)? != before {
                return Err(invalid("Mixed apply backup does not match captured source"));
            }
        } else if previous.is_some() {
            return Err(invalid(
                "Prepared mixed apply is missing an original-byte backup",
            ));
        } else {
            write_new_evidence(&backup, &before)?;
        }
    }
    let mut status = LocalApplyReceipt {
        schema: 4,
        run_id: receipt.id.clone(),
        candidate_number,
        path: None,
        before_sha256: None,
        after_sha256: None,
        files: changed.clone(),
        created_file: Some(created.clone()),
        baseline_sha256: Some(receipt.baseline_sha256.clone()),
        candidate_sha256: Some(expected_candidate_sha256.to_owned()),
        state: "prepared".into(),
        rollback_temporaries: Vec::new(),
        detail:
            "New file and existing edits are journaled; partial application may need roll-forward."
                .into(),
    };
    if previous.is_none() {
        save_status(&run_dir, &status)?;
    } else if let Some(saved) = &previous {
        status = saved.clone();
    }
    let source_states = classify_sources(&repository, &baseline, &candidate_dir, &files, &changed)?;
    if previous
        .as_ref()
        .is_some_and(|saved| saved.state == "applied")
    {
        if !target_applied || !source_states.iter().all(|applied| *applied) {
            return Err(invalid(
                "Previously applied mixed source changed; no repeat write occurred",
            ));
        }
        for file in &changed {
            if verify_recorded_temporary(&repository, file)? {
                std::fs::remove_file(repository.join(&file.temporary))?;
            }
        }
        if creation_temp_exists {
            std::fs::remove_file(repository.join(&created.temporary))?;
        }
        sync_source_parent(&target)?;
        sync_changed_parents(&repository, &changed)?;
        if let Some(saved) = previous {
            return Ok(saved);
        }
    }
    let created = if target_applied {
        created
    } else {
        stage_creation(&repository, &run_dir, &new_bytes, &mut status)?
    };
    for (file, applied) in changed.iter().zip(&source_states) {
        if *applied {
            continue;
        }
        let temporary = repository.join(&file.temporary);
        if verify_recorded_temporary(&repository, file)? {
            std::fs::remove_file(&temporary)?;
        }
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        output.write_all(&std::fs::read(candidate_dir.join(&file.path))?)?;
        #[cfg(not(windows))]
        output.set_permissions(
            std::fs::symlink_metadata(repository.join(&file.path))?.permissions(),
        )?;
        output.sync_all()?;
    }
    if checked_mixed_inventory(&repository, &changed, &created).await? != files
        || classify_sources(&repository, &baseline, &candidate_dir, &files, &changed).is_err()
        || !integrity::verify(&repository, &mut index, "before mixed publication").await
    {
        return Err(invalid(
            "Mixed apply needs recovery: staged bytes, source, or Git index changed",
        ));
    }
    let held_creation_anchor = if target_applied {
        None
    } else {
        Some(hold_created_anchor(&repository, &run_dir, &created)?)
    };
    if !target_applied {
        verify_unpublished_mixed_target(&repository, &run_dir, &created, path).await?;
        mark_creation_publication_attempted(&run_dir, &mut status)?;
        std::fs::hard_link(
            creation_anchor_path(&repository, &run_dir, &created)?,
            &target,
        )?;
    }
    if !classify_creation_target(&repository, &run_dir, &created)?
        || !integrity::verify(&repository, &mut index, "after mixed creation").await
    {
        return Err(invalid(
            "Mixed apply needs recovery: published file or Git index changed",
        ));
    }
    // Existing-source replacements may follow. Commit the new name first so
    // a later interrupted batch cannot retain edits while losing its creation.
    sync_source_parent(&target)?;
    for file in &changed {
        if std::fs::read(repository.join(&file.path))?
            == std::fs::read(candidate_dir.join(&file.path))?
        {
            continue;
        }
        if checked_mixed_inventory(&repository, &changed, &created).await? != files
            || !classify_creation_target(&repository, &run_dir, &created)?
            || classify_sources(&repository, &baseline, &candidate_dir, &files, &changed).is_err()
            || !integrity::verify(&repository, &mut index, "before mixed replacement").await
        {
            return Err(invalid(
                "Mixed apply needs recovery: source or Git index changed before replacement",
            ));
        }
        let source = repository.join(&file.path);
        if std::fs::read(&source)? != std::fs::read(baseline.join(&file.path))? {
            return Err(invalid(
                "Mixed apply needs recovery: target changed before replacement",
            ));
        }
        replace_source_path(&repository.join(&file.temporary), &source)?;
        if std::fs::read(&source)? != std::fs::read(candidate_dir.join(&file.path))?
            || !integrity::verify(&repository, &mut index, "after mixed replacement").await
        {
            return Err(invalid(
                "Mixed apply needs recovery: source or Git index changed after replacement",
            ));
        }
    }
    if checked_mixed_inventory(&repository, &changed, &created).await? != files
        || scoped_hash(&baseline, &files, Some(&created.path), true)? != receipt.baseline_sha256
        || scoped_hash(&candidate_dir, &files, Some(&created.path), true)?
            != expected_candidate_sha256
        || !classify_creation_target(&repository, &run_dir, &created)?
        || !classify_sources(&repository, &baseline, &candidate_dir, &files, &changed)?
            .iter()
            .all(|applied| *applied)
        || !integrity::verify(&repository, &mut index, "after mixed apply").await
    {
        return Err(invalid(
            "Mixed apply needs recovery: the selected changeset is not fully present",
        ));
    }
    drop(held_creation_anchor);
    for file in &changed {
        if verify_recorded_temporary(&repository, file)? {
            std::fs::remove_file(repository.join(&file.temporary))?;
        }
    }
    if verify_creation_temporary(&repository, &created)? {
        std::fs::remove_file(repository.join(&created.temporary))?;
    }
    sync_source_parent(&target)?;
    sync_changed_parents(&repository, &changed)?;
    status.state = "applied".into();
    status.detail = format!(
        "One new file and {} existing source edits match the reviewed candidate; original bytes remain in run backups and the Git index was unchanged.",
        changed.len()
    );
    save_status(&run_dir, &status)?;
    Ok(status)
}

/// Publish one explicitly named, verified new file without replacing anything
/// already at that path. Prepared schema-3 journals support safe roll-forward.
async fn apply_creation(
    receipt: &LocalRunReceipt,
    run_dir: &Path,
    candidate_number: u32,
    expected_candidate_sha256: &str,
    path: &Path,
    previous: Option<LocalApplyReceipt>,
) -> Result<LocalApplyReceipt> {
    if receipt.state != "review_ready" || receipt.selected_candidate != Some(candidate_number) {
        return Err(invalid(
            "Only the selected, verified candidate can be applied",
        ));
    }
    let candidate = receipt
        .candidates
        .iter()
        .find(|value| value.number == candidate_number)
        .ok_or_else(|| invalid("Selected candidate is missing"))?;
    if candidate.content_sha256.as_deref() != Some(expected_candidate_sha256)
        || candidate.stage != CandidateStage::Complete
        || !reviewed_checks_match(receipt, candidate)
    {
        return Err(invalid(
            "Reviewed candidate identity or verification is unavailable",
        ));
    }
    let mut index = receipt
        .git_index
        .as_ref()
        .filter(|value| value.status == CheckStatus::Passed && value.stage == "final review")
        .ok_or_else(|| invalid("Final Git index integrity was not proven"))?
        .clone();
    let repository = std::fs::canonicalize(&receipt.request.repository)?;
    let run_dir = std::fs::canonicalize(run_dir)?;
    if run_dir.starts_with(&repository)
        || run_dir.file_name().and_then(|name| name.to_str()) != Some(receipt.id.as_str())
    {
        return Err(invalid(
            "Run evidence is not outside its repository under its saved ID",
        ));
    }
    let baseline = run_dir.join("baseline");
    let candidate_dir = run_dir.join(format!("candidate-{candidate_number}"));
    if std::fs::canonicalize(&candidate_dir)? != std::fs::canonicalize(&candidate.directory)? {
        return Err(invalid(
            "Selected candidate directory does not match saved run evidence",
        ));
    }
    let target = repository.join(path);
    let parent = target
        .parent()
        .ok_or_else(|| invalid("Creation target has no parent directory"))?;
    let _held_parent = hold_apply_parent(parent)?;
    validate_creation_path(&repository, path, previous.is_some()).await?;
    let after = std::fs::read(candidate_dir.join(path))?;
    phonton_local::edit::canonical_new_file(
        path,
        std::str::from_utf8(&after).map_err(|_| invalid("Created file is not UTF-8 text"))?,
    )?;
    let mut expected_entry = creation_entry(path, &after, &receipt.id);
    expected_entry.anchor = Some(select_creation_anchor(&repository, &run_dir, parent)?);
    let expected_anchor = creation_anchor_path(&repository, &run_dir, &expected_entry)?;
    let _held_anchor_parent = hold_apply_parent(
        expected_anchor
            .parent()
            .ok_or_else(|| invalid("Creation anchor has no parent directory"))?,
    )?;
    if let Some(saved) = &previous {
        if saved.schema != 3
            || saved.run_id != receipt.id
            || saved.candidate_number != candidate_number
            || !saved
                .created_file
                .as_ref()
                .is_some_and(|entry| creation_entry_matches(entry, &expected_entry))
            || saved.baseline_sha256.as_deref() != Some(receipt.baseline_sha256.as_str())
            || saved.candidate_sha256.as_deref() != Some(expected_candidate_sha256)
            || !saved.files.is_empty()
            || !matches!(saved.state.as_str(), "prepared" | "applied")
        {
            return Err(invalid(
                "Existing creation journal identifies different candidate evidence",
            ));
        }
    }
    let entry = previous
        .as_ref()
        .and_then(|saved| saved.created_file.clone())
        .unwrap_or(expected_entry);
    let target_applied = classify_creation_target(&repository, &run_dir, &entry)?;
    let temp_exists = verify_creation_temporary(&repository, &entry)?;
    if previous.is_none() && (target_applied || temp_exists) {
        return Err(invalid(
            "Creation target or unrecorded temporary already exists",
        ));
    }
    if previous.is_some()
        && ((target_applied && !entry.publication_attempted)
            || (!target_applied && entry.publication_attempted)
            || (entry.identity.is_none() && temp_exists))
    {
        return Err(invalid(
            "Creation publication or temporary lacks ownership proof; manual recovery is required",
        ));
    }
    let excluded = BTreeSet::from([entry.path.clone(), entry.temporary.clone()]);
    let files = inventory_excluding(&repository, &excluded).await?;
    for source in &files {
        if std::fs::read(baseline.join(source))? != std::fs::read(candidate_dir.join(source))? {
            return Err(invalid(format!(
                "Creation candidate also edits existing source: {}",
                source.display()
            )));
        }
    }
    if scoped_hash(&baseline, &files, Some(&entry.path), true)? != receipt.baseline_sha256
        || scoped_hash(&candidate_dir, &files, Some(&entry.path), true)?
            != expected_candidate_sha256
        || baseline_diff_scoped(
            &baseline,
            &candidate_dir,
            &receipt.request.files,
            Some(&entry.path),
        )? != candidate.diff
        || captured_hash(&repository, &files)? != captured_hash(&baseline, &files)?
        || !integrity::verify(&repository, &mut index, "before creation apply").await
    {
        return Err(invalid(
            "Creation source, candidate, diff, or Git index changed since review",
        ));
    }
    let mut status = LocalApplyReceipt {
        schema: 3,
        run_id: receipt.id.clone(),
        candidate_number,
        path: None,
        before_sha256: None,
        after_sha256: None,
        files: Vec::new(),
        created_file: Some(entry.clone()),
        baseline_sha256: Some(receipt.baseline_sha256.clone()),
        candidate_sha256: Some(expected_candidate_sha256.into()),
        state: "prepared".into(),
        rollback_temporaries: Vec::new(),
        detail: "Creation target is journaled; publication may need recovery.".into(),
    };
    if let Some(saved) = &previous {
        status = saved.clone();
        if saved.state == "applied" {
            if !target_applied {
                return Err(invalid(
                    "Previously applied creation is missing; no repeat write occurred",
                ));
            }
            if temp_exists {
                std::fs::remove_file(repository.join(&entry.temporary))?;
            }
            sync_source_parent(&target)?;
            return Ok(saved.clone());
        }
        if target_applied {
            if temp_exists {
                std::fs::remove_file(repository.join(&entry.temporary))?;
            }
            sync_source_parent(&target)?;
            status.state = "applied".into();
            status.detail = "Recovered the exact reviewed creation after interruption; Git index and existing source still match.".into();
            save_status(&run_dir, &status)?;
            return Ok(status);
        }
    } else {
        save_status(&run_dir, &status)?;
    }
    let entry = stage_creation(&repository, &run_dir, &after, &mut status)?;
    let temporary = repository.join(&entry.temporary);
    if !verify_creation_temporary(&repository, &entry)?
        || classify_creation_target(&repository, &run_dir, &entry)?
        || validate_creation_path(&repository, path, false)
            .await
            .is_err()
        || inventory_excluding(&repository, &excluded).await? != files
        || captured_hash(&repository, &files)? != captured_hash(&baseline, &files)?
        || !integrity::verify(&repository, &mut index, "before creation publication").await
    {
        return Err(invalid(
            "Creation needs recovery: temporary, source, target, or Git index changed",
        ));
    }
    // Hard-link publication fails if another process created the target. A
    // rename fallback could overwrite it, so none is attempted.
    let held_anchor = hold_created_anchor(&repository, &run_dir, &entry)?;
    mark_creation_publication_attempted(&run_dir, &mut status)?;
    std::fs::hard_link(
        creation_anchor_path(&repository, &run_dir, &entry)?,
        &target,
    )?;
    if !classify_creation_target(&repository, &run_dir, &entry)?
        || validate_creation_path(&repository, path, true)
            .await
            .is_err()
        || inventory_excluding(&repository, &excluded).await? != files
        || captured_hash(&repository, &files)? != captured_hash(&baseline, &files)?
        || !integrity::verify(&repository, &mut index, "after creation publication").await
    {
        return Err(invalid(
            "Creation needs recovery: published source or Git index differs",
        ));
    }
    drop(held_anchor);
    if verify_creation_temporary(&repository, &entry)? {
        std::fs::remove_file(&temporary)?;
    }
    sync_source_parent(&target)?;
    status.state = "applied".into();
    status.detail = "One reviewed new file was published without replacing an existing path; existing source and Git index stayed unchanged.".into();
    save_status(&run_dir, &status)?;
    Ok(status)
}

fn rollback_temporary(path: &Path, run_id: &str, ordinal: usize) -> PathBuf {
    path.parent().unwrap_or_else(|| Path::new("")).join(format!(
        ".phonton-rollback-{}-{ordinal}.tmp",
        hash(run_id.as_bytes())
    ))
}

fn verify_rollback_temporary(repository: &Path, path: &Path, before: &[u8]) -> Result<bool> {
    if !existing_regular_file(&repository.join(path))? {
        return Ok(false);
    }
    if std::fs::read(repository.join(path))? != before {
        return Err(invalid(format!(
            "Recorded rollback temporary has unexpected bytes: {}",
            path.display()
        )));
    }
    Ok(true)
}

#[cfg(windows)]
async fn rollback_creation(
    receipt: &LocalRunReceipt,
    run_dir: &Path,
    repository: &Path,
    candidate: &CandidateEvidence,
    expected_candidate_sha256: &str,
    mut checked_index: GitIndexEvidence,
    mut status: LocalApplyReceipt,
) -> Result<LocalApplyReceipt> {
    let path = receipt
        .request
        .new_file
        .as_deref()
        .ok_or_else(|| invalid("Creation rollback has no reviewed target"))?;
    let candidate_number = candidate.number;
    let baseline = run_dir.join("baseline");
    let candidate_dir = run_dir.join(format!("candidate-{candidate_number}"));
    if std::fs::canonicalize(&candidate_dir)? != std::fs::canonicalize(&candidate.directory)? {
        return Err(invalid(
            "Selected candidate directory does not match saved run evidence",
        ));
    }
    let target = repository.join(path);
    let parent = target
        .parent()
        .ok_or_else(|| invalid("Creation target has no parent directory"))?;
    let _held_parent = hold_apply_parent(parent)?;
    validate_creation_recovery_path(repository, path).await?;
    let after = std::fs::read(candidate_dir.join(path))?;
    phonton_local::edit::canonical_new_file(
        path,
        std::str::from_utf8(&after).map_err(|_| invalid("Created file is not UTF-8 text"))?,
    )?;
    let mut expected_entry = creation_entry(path, &after, &receipt.id);
    expected_entry.anchor = Some(select_creation_anchor(repository, run_dir, parent)?);
    let expected_anchor = creation_anchor_path(repository, run_dir, &expected_entry)?;
    let _held_anchor_parent = hold_apply_parent(
        expected_anchor
            .parent()
            .ok_or_else(|| invalid("Creation anchor has no parent directory"))?,
    )?;
    let entry = status
        .created_file
        .as_ref()
        .ok_or_else(|| invalid("Creation rollback has no journaled target"))?;
    if status.schema != 3
        || status.run_id != receipt.id
        || status.candidate_number != candidate_number
        || !creation_entry_matches(entry, &expected_entry)
        || !entry.publication_attempted
        || entry.identity.is_none()
        || status.path.is_some()
        || status.before_sha256.is_some()
        || status.after_sha256.is_some()
        || !status.files.is_empty()
        || !status.rollback_temporaries.is_empty()
        || status.baseline_sha256.as_deref() != Some(receipt.baseline_sha256.as_str())
        || status.candidate_sha256.as_deref() != Some(expected_candidate_sha256)
        || !matches!(
            status.state.as_str(),
            "applied" | "rollback_prepared" | "rolled_back"
        )
        || (status.state == "applied" && entry.rollback_deletion_attempted)
        || (status.state == "rolled_back" && !entry.rollback_deletion_attempted)
    {
        return Err(invalid(
            "Creation rollback journal identifies different evidence",
        ));
    }
    if existing_regular_file(&repository.join(&entry.temporary))? {
        return Err(invalid(
            "Creation rollback found an unexpected forward temporary",
        ));
    }
    verify_created_ownership(repository, run_dir, entry, false)?;
    let target_present = existing_regular_file(&target)?;
    if !target_present
        && (status.state == "applied"
            || (status.state == "rollback_prepared" && !entry.rollback_deletion_attempted))
    {
        return Err(invalid(
            "Created target disappeared before a recorded deletion attempt; manual review is required",
        ));
    }
    if target_present && status.state == "rolled_back" {
        return Err(invalid(
            "Rolled-back creation target is occupied; no repeat deletion occurred",
        ));
    }
    if target_present && status.state == "rollback_prepared" && entry.rollback_deletion_attempted {
        return Err(invalid(
            "Created target is present after a recorded deletion attempt; its link history is ambiguous and needs manual review",
        ));
    }
    let excluded = BTreeSet::from([entry.path.clone()]);
    let files = inventory_excluding(repository, &excluded).await?;
    for source in &files {
        if std::fs::read(baseline.join(source))? != std::fs::read(candidate_dir.join(source))? {
            return Err(invalid("Creation candidate also edits existing source"));
        }
    }
    if scoped_hash(&baseline, &files, Some(&entry.path), true)? != receipt.baseline_sha256
        || scoped_hash(&candidate_dir, &files, Some(&entry.path), true)?
            != expected_candidate_sha256
        || baseline_diff_scoped(
            &baseline,
            &candidate_dir,
            &receipt.request.files,
            Some(&entry.path),
        )? != candidate.diff
        || captured_hash(repository, &files)? != captured_hash(&baseline, &files)?
        || !integrity::verify(repository, &mut checked_index, "before creation rollback").await
    {
        return Err(invalid(
            "Creation rollback source, candidate, diff, or Git index changed since review",
        ));
    }
    if status.state == "rolled_back" {
        sync_source_parent(&target)?;
        return Ok(status);
    }
    if target_present {
        let (target_file, anchor_file) =
            open_created_deletion_handles(&target, &expected_anchor, entry)?;
        if inventory_excluding(repository, &excluded).await? != files
            || captured_hash(repository, &files)? != captured_hash(&baseline, &files)?
            || !integrity::verify(
                repository,
                &mut checked_index,
                "immediately before creation rollback",
            )
            .await
        {
            return Err(invalid(
                "Creation rollback source or Git index changed before deletion",
            ));
        }
        if status.state == "applied" {
            status.state = "rollback_prepared".into();
            status.detail = "Creation rollback is journaled; the published target remains identity-checked before deletion.".into();
            save_status(run_dir, &status)?;
        }
        if !status
            .created_file
            .as_ref()
            .is_some_and(|created| created.rollback_deletion_attempted)
        {
            status
                .created_file
                .as_mut()
                .ok_or_else(|| invalid("Creation rollback lost its target"))?
                .rollback_deletion_attempted = true;
            save_status(run_dir, &status)?;
        }
        dispose_created_target(target_file)?;
        drop(anchor_file);
    }
    if existing_regular_file(&target)?
        || inventory_excluding(repository, &excluded).await? != files
        || captured_hash(repository, &files)? != captured_hash(&baseline, &files)?
        || verify_created_ownership(
            repository,
            run_dir,
            status
                .created_file
                .as_ref()
                .ok_or_else(|| invalid("Creation rollback lost its target"))?,
            false,
        )
        .is_err()
        || !integrity::verify(repository, &mut checked_index, "after creation rollback").await
    {
        return Err(invalid(
            "Creation rollback needs recovery: target, anchor, source, or Git index changed",
        ));
    }
    sync_source_parent(&target)?;
    status.state = "rolled_back".into();
    status.detail = "The identity-checked created file is absent; existing source and Git index stayed unchanged. The retained anchor preserves rollback evidence.".into();
    save_status(run_dir, &status)?;
    Ok(status)
}

async fn checked_rollback_inventory(
    repository: &Path,
    baseline: &Path,
    changed: &[LocalApplyFile],
    temporaries: &[PathBuf],
    batch: bool,
) -> Result<Vec<PathBuf>> {
    let mut excluded = BTreeSet::new();
    for (file, temporary) in changed.iter().zip(temporaries) {
        let before = std::fs::read(baseline.join(&file.path))?;
        verify_rollback_temporary(repository, temporary, &before)?;
        excluded.insert(temporary.clone());
        if batch {
            verify_recorded_temporary(repository, file)?;
            excluded.insert(file.temporary.clone());
        }
    }
    inventory_excluding(repository, &excluded).await
}

#[cfg(windows)]
async fn checked_mixed_rollback_inventory(
    repository: &Path,
    created: &LocalApplyCreate,
    changed: &[LocalApplyFile],
    temporaries: &[PathBuf],
    baseline: &Path,
) -> Result<Vec<PathBuf>> {
    let mut excluded = BTreeSet::from([created.path.clone()]);
    for (file, temporary) in changed.iter().zip(temporaries) {
        if verify_recorded_temporary(repository, file)? {
            return Err(invalid(
                "Mixed rollback found an unexpected forward temporary",
            ));
        }
        let before = std::fs::read(baseline.join(&file.path))?;
        verify_rollback_temporary(repository, temporary, &before)?;
        excluded.insert(temporary.clone());
    }
    inventory_excluding(repository, &excluded).await
}

/// Restore the existing edits before disposing the witnessed creation. An
/// interrupted source restore can resume from either saved source state; once
/// deletion is attempted, only an absent target and fully restored sources
/// can finish recovery. The journal is not an atomic multi-file transaction.
#[cfg(windows)]
async fn rollback_mixed(
    receipt: &LocalRunReceipt,
    run_dir: &Path,
    repository: &Path,
    candidate: &CandidateEvidence,
    expected_candidate_sha256: &str,
    mut checked_index: GitIndexEvidence,
    mut status: LocalApplyReceipt,
) -> Result<LocalApplyReceipt> {
    let path = receipt
        .request
        .new_file
        .as_deref()
        .ok_or_else(|| invalid("Mixed rollback has no reviewed creation target"))?;
    let baseline = run_dir.join("baseline");
    let candidate_dir = run_dir.join(format!("candidate-{}", candidate.number));
    if std::fs::canonicalize(&candidate_dir)? != std::fs::canonicalize(&candidate.directory)? {
        return Err(invalid(
            "Selected candidate directory does not match saved run evidence",
        ));
    }
    let target = repository.join(path);
    let parent = target
        .parent()
        .ok_or_else(|| invalid("Creation target has no parent directory"))?;
    let _held_parent = hold_apply_parent(parent)?;
    validate_creation_recovery_path(repository, path).await?;
    let after = std::fs::read(candidate_dir.join(path))?;
    phonton_local::edit::canonical_new_file(
        path,
        std::str::from_utf8(&after).map_err(|_| invalid("Created file is not UTF-8 text"))?,
    )?;
    let mut expected_created = creation_entry(path, &after, &receipt.id);
    expected_created.anchor = Some(select_creation_anchor(repository, run_dir, parent)?);
    let anchor = creation_anchor_path(repository, run_dir, &expected_created)?;
    let _held_anchor_parent = hold_apply_parent(
        anchor
            .parent()
            .ok_or_else(|| invalid("Creation anchor has no parent directory"))?,
    )?;
    let created = status
        .created_file
        .clone()
        .ok_or_else(|| invalid("Mixed rollback has no journaled creation"))?;
    if status.schema != 4
        || status.run_id != receipt.id
        || status.candidate_number != candidate.number
        || !creation_entry_matches(&created, &expected_created)
        || !created.publication_attempted
        || created.identity.is_none()
        || status.path.is_some()
        || status.before_sha256.is_some()
        || status.after_sha256.is_some()
        || status.baseline_sha256.as_deref() != Some(receipt.baseline_sha256.as_str())
        || status.candidate_sha256.as_deref() != Some(expected_candidate_sha256)
        || status.files.is_empty()
        || status.files.len() > 3
        || !matches!(
            status.state.as_str(),
            "applied" | "rollback_prepared" | "rolled_back"
        )
        || (status.state == "applied" && created.rollback_deletion_attempted)
        || (status.state == "rolled_back" && !created.rollback_deletion_attempted)
    {
        return Err(invalid("Mixed rollback journal identifies different evidence; finish an interrupted Apply before rollback"));
    }
    if existing_regular_file(&repository.join(&created.temporary))? {
        return Err(invalid(
            "Mixed rollback found an unexpected creation temporary",
        ));
    }
    verify_created_ownership(repository, run_dir, &created, false)?;
    let target_present = existing_regular_file(&target)?;
    if !target_present
        && (status.state == "applied"
            || (status.state == "rollback_prepared" && !created.rollback_deletion_attempted))
    {
        return Err(invalid("Created target disappeared before a recorded deletion attempt; manual review is required"));
    }
    if target_present && created.rollback_deletion_attempted {
        return Err(invalid("Created target is present after a recorded deletion attempt; its link history is ambiguous and needs manual review"));
    }
    if target_present && status.state == "rolled_back" {
        return Err(invalid(
            "Rolled-back creation target is occupied; no repeat deletion occurred",
        ));
    }
    if target_present {
        verify_created_ownership(repository, run_dir, &created, true)?;
    }

    let rollback_started = status.state != "applied";
    let temporaries: Vec<PathBuf> = status
        .files
        .iter()
        .enumerate()
        .map(|(ordinal, file)| rollback_temporary(&file.path, &receipt.id, ordinal))
        .collect();
    if (rollback_started && status.rollback_temporaries != temporaries)
        || (!rollback_started && !status.rollback_temporaries.is_empty())
    {
        return Err(invalid(
            "Mixed rollback journal has invalid temporary paths",
        ));
    }
    if !rollback_started {
        for (temporary, file) in temporaries.iter().zip(&status.files) {
            if verify_rollback_temporary(
                repository,
                temporary,
                &std::fs::read(baseline.join(&file.path))?,
            )? {
                return Err(invalid(
                    "Unrecorded mixed rollback temporary already exists",
                ));
            }
        }
    }
    if created.rollback_deletion_attempted {
        for (temporary, file) in temporaries.iter().zip(&status.files) {
            if verify_rollback_temporary(
                repository,
                temporary,
                &std::fs::read(baseline.join(&file.path))?,
            )? {
                return Err(invalid(
                    "Deletion was attempted with an unexpected rollback temporary",
                ));
            }
        }
    }
    for (ordinal, file) in status.files.iter().enumerate() {
        phonton_local::edit::safe_relative_path(&file.path.to_string_lossy())?;
        let expected = batch_file(file.path.clone(), &[], &[], &receipt.id, ordinal);
        if file.backup != expected.backup
            || file.temporary != expected.temporary
            || !receipt.request.editable_existing.contains(&file.path)
            || file.path == created.path
        {
            return Err(invalid(
                "Mixed rollback journal has an invalid source or temporary path",
            ));
        }
    }
    let files = checked_mixed_rollback_inventory(
        repository,
        &created,
        &status.files,
        &temporaries,
        &baseline,
    )
    .await?;
    if scoped_hash(&baseline, &files, Some(&created.path), true)? != receipt.baseline_sha256
        || scoped_hash(&candidate_dir, &files, Some(&created.path), true)?
            != expected_candidate_sha256
        || baseline_diff_scoped(
            &baseline,
            &candidate_dir,
            &receipt.request.files,
            Some(&created.path),
        )? != candidate.diff
    {
        return Err(invalid("Saved baseline, mixed candidate, or diff changed"));
    }
    let mut changed = Vec::new();
    for source in &files {
        let before = std::fs::read(baseline.join(source))?;
        let after = std::fs::read(candidate_dir.join(source))?;
        if before != after {
            if !receipt.request.editable_existing.contains(source) {
                return Err(invalid(
                    "Mixed candidate changed source outside reviewed edit scope",
                ));
            }
            changed.push(batch_file(
                source.clone(),
                &before,
                &after,
                &receipt.id,
                changed.len(),
            ));
        }
    }
    if changed.is_empty() || changed.len() > 3 || status.files != changed {
        return Err(invalid(
            "Mixed rollback journal identifies different source bytes",
        ));
    }
    let _held_source_parents =
        hold_source_parents(repository, changed.iter().map(|file| file.path.as_path()))?;
    let mut originals = Vec::with_capacity(changed.len());
    for file in &changed {
        let before = std::fs::read(baseline.join(&file.path))?;
        let backup = run_dir.join(&file.backup);
        if !existing_regular_file(&backup)? || std::fs::read(&backup)? != before {
            return Err(invalid(format!(
                "Original-byte backup is missing or changed: {}",
                file.path.display()
            )));
        }
        originals.push(before);
    }
    if !integrity::verify(repository, &mut checked_index, "before mixed rollback").await {
        return Err(invalid("Original Git index changed since final review"));
    }
    let states = classify_sources(repository, &baseline, &candidate_dir, &files, &changed)?;
    if status.state == "applied" && states.iter().any(|state| !*state) {
        return Err(invalid("Applied mixed source changed before rollback"));
    }
    if created.rollback_deletion_attempted && states.iter().any(|state| *state) {
        return Err(invalid("Created target was deleted before all source was restored; manual recovery is required"));
    }
    if status.state == "rolled_back" {
        for (temporary, before) in temporaries.iter().zip(&originals) {
            if verify_rollback_temporary(repository, temporary, before)? {
                return Err(invalid("Rolled-back journal has an unexpected temporary"));
            }
        }
        sync_source_parent(&target)?;
        sync_changed_parents(repository, &changed)?;
        return Ok(status);
    }

    // Pin the exact published target and its witness throughout source restore.
    // A present path after the deletion-attempt marker is deliberately refused.
    let handles = if target_present {
        Some(open_created_deletion_handles(&target, &anchor, &created)?)
    } else {
        None
    };
    if status.state == "applied" {
        status.state = "rollback_prepared".into();
        status.rollback_temporaries = temporaries.clone();
        status.detail = "Mixed rollback is journaled; original source is restored before created-file deletion.".into();
        save_status(run_dir, &status)?;
    }
    if !created.rollback_deletion_attempted {
        for (ordinal, file) in changed.iter().enumerate() {
            let before = &originals[ordinal];
            let after = std::fs::read(candidate_dir.join(&file.path))?;
            let temporary = repository.join(&temporaries[ordinal]);
            if checked_mixed_rollback_inventory(
                repository,
                &created,
                &changed,
                &temporaries,
                &baseline,
            )
            .await?
                != files
                || classify_sources(repository, &baseline, &candidate_dir, &files, &changed)
                    .is_err()
                || !integrity::verify(
                    repository,
                    &mut checked_index,
                    "before mixed rollback replacement",
                )
                .await
            {
                return Err(invalid(
                    "Mixed rollback needs recovery: source inventory or Git index changed",
                ));
            }
            let source = repository.join(&file.path);
            if std::fs::read(&source)? == *before {
                continue;
            }
            if std::fs::read(&source)? != after {
                return Err(invalid(
                    "Mixed rollback needs recovery: source changed before replacement",
                ));
            }
            if verify_rollback_temporary(repository, &temporaries[ordinal], before)? {
                std::fs::remove_file(&temporary)?;
            }
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            output.write_all(before)?;
            output.sync_all()?;
            drop(output);
            if !verify_rollback_temporary(repository, &temporaries[ordinal], before)?
                || checked_mixed_rollback_inventory(
                    repository,
                    &created,
                    &changed,
                    &temporaries,
                    &baseline,
                )
                .await?
                    != files
                || std::fs::read(&source)? != after
                || !integrity::verify(
                    repository,
                    &mut checked_index,
                    "immediately before mixed rollback replacement",
                )
                .await
            {
                return Err(invalid(
                    "Mixed rollback needs recovery: source, temporary, or Git index changed",
                ));
            }
            replace_source_path(&temporary, &source)?;
            if std::fs::read(&source)? != *before
                || !integrity::verify(
                    repository,
                    &mut checked_index,
                    "after mixed rollback replacement",
                )
                .await
            {
                return Err(invalid(
                    "Mixed rollback needs recovery: source or Git index changed after replacement",
                ));
            }
        }
    }
    if checked_mixed_rollback_inventory(repository, &created, &changed, &temporaries, &baseline)
        .await?
        != files
        || classify_sources(repository, &baseline, &candidate_dir, &files, &changed)?
            .iter()
            .any(|state| *state)
        || !integrity::verify(
            repository,
            &mut checked_index,
            "before mixed creation disposal",
        )
        .await
    {
        return Err(invalid(
            "Mixed rollback needs recovery: original source is not fully present",
        ));
    }
    for (temporary, before) in temporaries.iter().zip(&originals) {
        if verify_rollback_temporary(repository, temporary, before)? {
            std::fs::remove_file(repository.join(temporary))?;
        }
    }
    // The deletion-attempt marker forbids recovering candidate-state source.
    // Commit every restored source name before that marker can be persisted.
    sync_changed_parents(repository, &changed)?;
    if !created.rollback_deletion_attempted {
        status
            .created_file
            .as_mut()
            .ok_or_else(|| invalid("Mixed rollback lost its target"))?
            .rollback_deletion_attempted = true;
        save_status(run_dir, &status)?;
        let (target_file, anchor_file) =
            handles.ok_or_else(|| invalid("Mixed rollback lost its published target"))?;
        dispose_created_target(target_file)?;
        drop(anchor_file);
    }
    if existing_regular_file(&target)?
        || checked_mixed_rollback_inventory(repository, &created, &changed, &temporaries, &baseline)
            .await?
            != files
        || classify_sources(repository, &baseline, &candidate_dir, &files, &changed)?
            .iter()
            .any(|state| *state)
        || verify_created_ownership(repository, run_dir, &created, false).is_err()
        || !integrity::verify(repository, &mut checked_index, "after mixed rollback").await
    {
        return Err(invalid(
            "Mixed rollback needs recovery: target, anchor, source, or Git index changed",
        ));
    }
    sync_source_parent(&target)?;
    sync_changed_parents(repository, &changed)?;
    status.state = "rolled_back".into();
    status.detail = format!("Restored {} original source file(s) and removed the witnessed created file; Git index stayed unchanged.", changed.len());
    save_status(run_dir, &status)?;
    Ok(status)
}

/// Explicitly restore original bytes from a supported Apply journal.
/// This refuses observed later edits and preserves the original Git index. A
/// multi-file rollback is journaled and resumable, but is not atomic as a set.
pub async fn rollback_selected(
    receipt: &LocalRunReceipt,
    run_dir: &Path,
    candidate_number: u32,
    expected_candidate_sha256: &str,
) -> Result<LocalApplyReceipt> {
    if receipt.state != "review_ready" || receipt.selected_candidate != Some(candidate_number) {
        return Err(invalid(
            "Only the selected, verified candidate can be rolled back",
        ));
    }
    let candidate = receipt
        .candidates
        .iter()
        .find(|value| value.number == candidate_number)
        .ok_or_else(|| invalid("Selected candidate is missing"))?;
    if candidate.content_sha256.as_deref() != Some(expected_candidate_sha256)
        || candidate.stage != CandidateStage::Complete
        || !reviewed_checks_match(receipt, candidate)
    {
        return Err(invalid(
            "Reviewed candidate identity or verification is unavailable",
        ));
    }
    let mut checked_index = receipt
        .git_index
        .as_ref()
        .filter(|value| value.status == CheckStatus::Passed && value.stage == "final review")
        .ok_or_else(|| invalid("Final Git index integrity was not proven"))?
        .clone();
    let repository = std::fs::canonicalize(&receipt.request.repository)?;
    let run_dir = std::fs::canonicalize(run_dir)?;
    if run_dir.starts_with(&repository)
        || run_dir.file_name().and_then(|name| name.to_str()) != Some(receipt.id.as_str())
    {
        return Err(invalid(
            "Run evidence is not outside its repository under its saved ID",
        ));
    }
    let mut status =
        read_status(&run_dir)?.ok_or_else(|| invalid("No Apply journal to roll back"))?;
    if receipt.request.new_file.is_some() {
        #[cfg(windows)]
        return if receipt.request.editable_existing.is_empty() {
            rollback_creation(
                receipt,
                &run_dir,
                &repository,
                candidate,
                expected_candidate_sha256,
                checked_index,
                status,
            )
            .await
        } else {
            rollback_mixed(
                receipt,
                &run_dir,
                &repository,
                candidate,
                expected_candidate_sha256,
                checked_index,
                status,
            )
            .await
        };
        #[cfg(not(windows))]
        return Err(invalid(
            "Created-file rollback is unavailable on this platform",
        ));
    }
    if !matches!(status.schema, 1 | 2)
        || status.run_id != receipt.id
        || status.candidate_number != candidate_number
        || status.created_file.is_some()
        || !matches!(
            status.state.as_str(),
            "prepared" | "applied" | "rollback_prepared" | "rolled_back"
        )
    {
        return Err(invalid(
            "Apply journal cannot be rolled back as existing source",
        ));
    }
    let baseline = run_dir.join("baseline");
    let candidate_dir = run_dir.join(format!("candidate-{candidate_number}"));
    if std::fs::canonicalize(&candidate_dir)? != std::fs::canonicalize(&candidate.directory)? {
        return Err(invalid(
            "Selected candidate directory does not match saved run evidence",
        ));
    }
    let rollback_started = matches!(status.state.as_str(), "rollback_prepared" | "rolled_back");
    let mut excluded = BTreeSet::new();
    if status.schema == 2 {
        if status.baseline_sha256.as_deref() != Some(receipt.baseline_sha256.as_str())
            || status.candidate_sha256.as_deref() != Some(expected_candidate_sha256)
            || status.files.is_empty()
            || status.files.len() > receipt.request.files.len()
        {
            return Err(invalid("Apply journal has different tree identity"));
        }
        for (ordinal, file) in status.files.iter().enumerate() {
            phonton_local::edit::safe_relative_path(&file.path.to_string_lossy())?;
            let expected = batch_file(file.path.clone(), &[], &[], &receipt.id, ordinal);
            if file.backup != expected.backup
                || file.temporary != expected.temporary
                || !receipt.request.files.contains(&file.path)
            {
                return Err(invalid(
                    "Apply journal has an invalid source or temporary path",
                ));
            }
            if status.state == "prepared" || rollback_started {
                verify_recorded_temporary(&repository, file)?;
                excluded.insert(file.temporary.clone());
            } else if existing_regular_file(&repository.join(&file.temporary))? {
                return Err(invalid(
                    "Applied journal has an unexpected forward temporary",
                ));
            }
        }
    } else if !status.files.is_empty()
        || status.path.is_none()
        || status.baseline_sha256.is_some()
        || status.candidate_sha256.is_some()
    {
        return Err(invalid(
            "One-file Apply journal has invalid source identity",
        ));
    }
    let recorded_temporaries = status.rollback_temporaries.clone();
    let journal_paths: Vec<&Path> = if status.schema == 1 {
        status.path.iter().map(PathBuf::as_path).collect()
    } else {
        status
            .files
            .iter()
            .map(|file| file.path.as_path())
            .collect()
    };
    if rollback_started
        && recorded_temporaries
            != journal_paths
                .iter()
                .enumerate()
                .map(|(ordinal, path)| rollback_temporary(path, &receipt.id, ordinal))
                .collect::<Vec<_>>()
    {
        return Err(invalid("Rollback journal has invalid temporary paths"));
    }
    if rollback_started {
        for path in &recorded_temporaries {
            excluded.insert(path.clone());
        }
    } else if !recorded_temporaries.is_empty() {
        return Err(invalid(
            "Rollback temporaries appeared before rollback was journaled",
        ));
    }
    let files = inventory_excluding(&repository, &excluded).await?;
    if content_hash(&baseline, &files)? != receipt.baseline_sha256
        || content_hash(&candidate_dir, &files)? != expected_candidate_sha256
        || baseline_diff(&baseline, &candidate_dir, &receipt.request.files)? != candidate.diff
    {
        return Err(invalid(
            "Saved baseline, selected candidate, or diff changed",
        ));
    }
    let mut changed = Vec::new();
    for path in &files {
        let before = std::fs::read(baseline.join(path))?;
        let after = std::fs::read(candidate_dir.join(path))?;
        if before != after {
            if !receipt.request.files.contains(path) {
                return Err(invalid(
                    "Selected candidate changed source outside reviewed scope",
                ));
            }
            changed.push(batch_file(
                path.clone(),
                &before,
                &after,
                &receipt.id,
                changed.len(),
            ));
        }
    }
    if changed.is_empty() || changed.len() > receipt.request.files.len() {
        return Err(invalid("Selected candidate has no bounded source changes"));
    }
    if status.schema == 1 {
        if changed.len() != 1
            || status.path.as_ref() != Some(&changed[0].path)
            || status.before_sha256.as_deref() != Some(changed[0].before_sha256.as_str())
            || status.after_sha256.as_deref() != Some(changed[0].after_sha256.as_str())
        {
            return Err(invalid(
                "One-file Apply journal identifies different source bytes",
            ));
        }
        changed[0].backup = PathBuf::from("apply-backup.bin");
    } else if status.files != changed {
        return Err(invalid(
            "Batch Apply journal identifies different source bytes",
        ));
    }
    let temporaries: Vec<PathBuf> = changed
        .iter()
        .enumerate()
        .map(|(ordinal, file)| rollback_temporary(&file.path, &receipt.id, ordinal))
        .collect();
    if rollback_started && recorded_temporaries != temporaries {
        return Err(invalid("Rollback journal has invalid temporary paths"));
    }
    let _held_source_parents =
        hold_source_parents(&repository, changed.iter().map(|file| file.path.as_path()))?;
    let mut original_bytes = Vec::with_capacity(changed.len());
    for (file, temporary) in changed.iter().zip(&temporaries) {
        let before = std::fs::read(baseline.join(&file.path))?;
        let backup = run_dir.join(&file.backup);
        if !existing_regular_file(&backup)? {
            return Err(invalid(format!(
                "Original-byte backup is missing or changed: {}",
                file.path.display()
            )));
        }
        let backed_up = std::fs::read(&backup)?;
        if backed_up != before {
            return Err(invalid(format!(
                "Original-byte backup is missing or changed: {}",
                file.path.display()
            )));
        }
        original_bytes.push(backed_up);
        if rollback_started {
            verify_rollback_temporary(&repository, temporary, &before)?;
        } else if existing_regular_file(&repository.join(temporary))? {
            return Err(invalid("Unrecorded rollback temporary already exists"));
        }
    }
    if !integrity::verify(&repository, &mut checked_index, "before rollback").await {
        return Err(invalid("Original Git index changed since final review"));
    }
    let states = classify_sources(&repository, &baseline, &candidate_dir, &files, &changed)?;
    if status.state == "rolled_back" {
        if states.iter().any(|state| *state) {
            return Err(invalid(
                "Rolled-back source changed; no repeat write occurred",
            ));
        }
        for (file, temporary) in changed.iter().zip(&temporaries) {
            if status.schema == 2 && verify_recorded_temporary(&repository, file)? {
                return Err(invalid(
                    "Rolled-back journal has an unexpected forward temporary",
                ));
            }
            if verify_rollback_temporary(
                &repository,
                temporary,
                &std::fs::read(baseline.join(&file.path))?,
            )? {
                return Err(invalid("Rolled-back journal has an unexpected temporary"));
            }
        }
        sync_changed_parents(&repository, &changed)?;
        return Ok(status);
    }
    if !rollback_started {
        status.rollback_temporaries = temporaries.clone();
        status.state = "rollback_prepared".into();
        status.detail =
            "Original-byte restore is journaled; interrupted files may be in either saved state."
                .into();
        save_status(&run_dir, &status)?;
    }
    for file in &changed {
        if status.schema == 2 && verify_recorded_temporary(&repository, file)? {
            std::fs::remove_file(repository.join(&file.temporary))?;
        }
    }
    for (ordinal, file) in changed.iter().enumerate() {
        let before = &original_bytes[ordinal];
        let after = std::fs::read(candidate_dir.join(&file.path))?;
        let temporary = repository.join(&temporaries[ordinal]);
        if checked_rollback_inventory(
            &repository,
            &baseline,
            &changed,
            &temporaries,
            status.schema == 2,
        )
        .await?
            != files
            || classify_sources(&repository, &baseline, &candidate_dir, &files, &changed).is_err()
            || !integrity::verify(
                &repository,
                &mut checked_index,
                "before rollback replacement",
            )
            .await
        {
            return Err(invalid(
                "Rollback needs recovery: source inventory or Git index changed",
            ));
        }
        let target = repository.join(&file.path);
        if std::fs::read(&target)?.as_slice() == before.as_slice() {
            continue;
        }
        if std::fs::read(&target)? != after {
            return Err(invalid(
                "Rollback needs recovery: target changed before replacement",
            ));
        }
        // A recovered temporary may be a hard link. Unlink it, then create a
        // fresh exclusive file rather than writing or chmodding through it.
        if verify_rollback_temporary(&repository, &temporaries[ordinal], before)? {
            std::fs::remove_file(&temporary)?;
        }
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        output.write_all(before)?;
        #[cfg(not(windows))]
        output.set_permissions(std::fs::symlink_metadata(&target)?.permissions())?;
        output.sync_all()?;
        drop(output);
        if !verify_rollback_temporary(&repository, &temporaries[ordinal], before)?
            || checked_rollback_inventory(
                &repository,
                &baseline,
                &changed,
                &temporaries,
                status.schema == 2,
            )
            .await?
                != files
            || std::fs::read(&target)? != after
            || !integrity::verify(
                &repository,
                &mut checked_index,
                "immediately before rollback replacement",
            )
            .await
        {
            return Err(invalid(
                "Rollback needs recovery: target, temporary, or Git index changed",
            ));
        }
        replace_source_path(&temporary, &target)?;
        if std::fs::read(&target)?.as_slice() != before.as_slice()
            || !integrity::verify(
                &repository,
                &mut checked_index,
                "after rollback replacement",
            )
            .await
        {
            return Err(invalid(
                "Rollback needs recovery: source or Git index changed after replacement",
            ));
        }
    }
    if checked_rollback_inventory(
        &repository,
        &baseline,
        &changed,
        &temporaries,
        status.schema == 2,
    )
    .await?
        != files
        || classify_sources(&repository, &baseline, &candidate_dir, &files, &changed)?
            .iter()
            .any(|state| *state)
        || !integrity::verify(&repository, &mut checked_index, "after rollback").await
    {
        return Err(invalid(
            "Rollback needs recovery: original source set is not fully present",
        ));
    }
    for (file, temporary) in changed.iter().zip(&temporaries) {
        let before = std::fs::read(baseline.join(&file.path))?;
        if verify_rollback_temporary(&repository, temporary, &before)? {
            std::fs::remove_file(repository.join(temporary))?;
        }
    }
    sync_changed_parents(&repository, &changed)?;
    status.state = "rolled_back".into();
    status.detail = format!(
        "Restored {} existing source file(s) to original bytes; the Git index stayed unchanged. Observed later edits are refused.",
        changed.len()
    );
    save_status(&run_dir, &status)?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_run::{baseline_diff, copy_files};
    use phonton_types::{
        local::{HardwareSnapshot, ModelProfile},
        local_run::{
            CandidateEvidence, CheckEvidence, CheckPurpose, LocalCheck, LocalRunRequest,
            SearchBudget,
        },
    };
    use std::path::PathBuf;

    fn file_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_file(target, link);
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(target, link);
        match result {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => false,
            Err(error) => panic!("Cannot create test symlink: {error}"),
        }
    }

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.autocrlf=false",
                "-C",
            ])
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    async fn fixture() -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        LocalRunReceipt,
        Vec<PathBuf>,
    ) {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        git(&repository, &["init", "--quiet"]);
        std::fs::write(repository.join("code.py"), "value = 1\n").unwrap();
        std::fs::write(repository.join("notes.md"), "staged copy\n").unwrap();
        git(&repository, &["add", "code.py", "notes.md"]);
        std::fs::write(repository.join("notes.md"), "dirty worktree copy\n").unwrap();
        std::fs::write(repository.join("loose.txt"), "untracked copy\n").unwrap();
        let id = "11111111-1111-4111-8111-111111111111";
        let run_dir = root.path().join(id);
        std::fs::create_dir(&run_dir).unwrap();
        let baseline = run_dir.join("baseline");
        let files = inventory(&repository).await.unwrap();
        copy_files(&repository, &baseline, &files).await.unwrap();
        let candidate_dir = run_dir.join("candidate-1");
        copy_files(&baseline, &candidate_dir, &files).await.unwrap();
        std::fs::write(candidate_dir.join("code.py"), "value = 2\n").unwrap();
        let baseline_sha256 = content_hash(&baseline, &files).unwrap();
        let candidate_sha256 = content_hash(&candidate_dir, &files).unwrap();
        let mut git_index = integrity::capture(&repository).await.unwrap();
        assert!(integrity::verify(&repository, &mut git_index, "final review").await);
        let request = LocalRunRequest {
            goal: "Change value".into(),
            repository: repository.clone(),
            files: vec!["code.py".into()],
            new_file: None,
            editable_existing: vec![],
            checks: vec![LocalCheck {
                program: "python".into(),
                args: vec!["check.py".into()],
            }],
            preparation: None,
            approve_host_execution: true,
            allow_unverified_runtime: false,
            budget: SearchBudget::default(),
            expected_source_hashes: Default::default(),
            expected_baseline_sha256: None,
        };
        let receipt = LocalRunReceipt {
            schema: 4,
            id: id.into(),
            state: "review_ready".into(),
            request,
            profile: ModelProfile {
                schema: 1,
                model: "test-model".into(),
                digest: "test-digest".into(),
                runtime_version: "test-runtime".into(),
                endpoint: "http://127.0.0.1:11434".into(),
                context_tokens: 4096,
                output_tokens: 512,
                protocol: None,
                thinking: None,
                probes: vec![],
                hardware: HardwareSnapshot::default(),
                measured_at_unix: 0,
            },
            runtime_origin: phonton_types::local_run::RuntimeOrigin::Unknown,
            hardware: HardwareSnapshot::default(),
            resident_reuse: None,
            baseline_sha256,
            baseline_checks: vec![],
            candidates: vec![CandidateEvidence {
                number: 1,
                stage: phonton_types::local_run::CandidateStage::Complete,
                approach: "test".into(),
                directory: candidate_dir.clone(),
                content_sha256: Some(candidate_sha256),
                raw_output: String::new(),
                input_tokens: None,
                output_tokens: None,
                elapsed_ms: 0,
                checks: vec![CheckEvidence {
                    check: Some(LocalCheck {
                        program: "python".into(),
                        args: vec!["check.py".into()],
                    }),
                    purpose: CheckPurpose::Verification,
                    status: CheckStatus::Passed,
                    exit_code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                    detail: "fixture check".into(),
                    elapsed_ms: 0,
                }],
                rejection: None,
                diff: baseline_diff(&baseline, &candidate_dir, &["code.py".into()]).unwrap(),
                context: None,
                decision: None,
            }],
            hypotheses: vec![],
            selected_candidate: Some(1),
            checks_used: 1,
            generated_tokens_reserved: 0,
            model_calls_reserved: 0,
            elapsed_ms: 0,
            known_gaps: vec![],
            contract: None,
            git_index: Some(git_index),
        };
        (root, repository, run_dir, receipt, files)
    }

    async fn two_file_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, LocalRunReceipt) {
        let (root, repository, run_dir, mut receipt, files) = fixture().await;
        let candidate_dir = run_dir.join("candidate-1");
        std::fs::write(candidate_dir.join("notes.md"), "candidate note\n").unwrap();
        receipt.request.files.push("notes.md".into());
        receipt.candidates[0].content_sha256 = Some(content_hash(&candidate_dir, &files).unwrap());
        receipt.candidates[0].diff = baseline_diff(
            &run_dir.join("baseline"),
            &candidate_dir,
            &receipt.request.files,
        )
        .unwrap();
        (root, repository, run_dir, receipt)
    }

    async fn creation_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, LocalRunReceipt) {
        let (root, repository, run_dir, mut receipt, files) = fixture().await;
        let path = PathBuf::from("src/new.py");
        std::fs::create_dir(repository.join("src")).unwrap();
        std::fs::create_dir(run_dir.join("baseline/src")).unwrap();
        let candidate_dir = run_dir.join("candidate-1");
        std::fs::create_dir(candidate_dir.join("src")).unwrap();
        std::fs::write(candidate_dir.join("code.py"), "value = 1\n").unwrap();
        std::fs::write(candidate_dir.join(&path), "value = 2\n").unwrap();
        receipt.request.goal = "Create src/new.py".into();
        receipt.request.new_file = Some(path.clone());
        receipt.baseline_sha256 =
            scoped_hash(&run_dir.join("baseline"), &files, Some(&path), true).unwrap();
        receipt.candidates[0].content_sha256 =
            Some(scoped_hash(&candidate_dir, &files, Some(&path), true).unwrap());
        receipt.candidates[0].diff = baseline_diff_scoped(
            &run_dir.join("baseline"),
            &candidate_dir,
            &receipt.request.files,
            Some(&path),
        )
        .unwrap();
        (root, repository, run_dir, receipt)
    }

    async fn mixed_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, LocalRunReceipt) {
        let (root, repository, run_dir, mut receipt) = creation_fixture().await;
        let files = inventory(&repository).await.unwrap();
        let candidate_dir = run_dir.join("candidate-1");
        std::fs::write(candidate_dir.join("code.py"), "value = 3\n").unwrap();
        receipt.request.goal = "Create a helper and wire the caller".into();
        receipt.request.editable_existing = vec!["code.py".into()];
        let path = receipt.request.new_file.as_ref().unwrap();
        receipt.candidates[0].content_sha256 =
            Some(scoped_hash(&candidate_dir, &files, Some(path), true).unwrap());
        receipt.candidates[0].diff = baseline_diff_scoped(
            &run_dir.join("baseline"),
            &candidate_dir,
            &receipt.request.files,
            Some(path),
        )
        .unwrap();
        (root, repository, run_dir, receipt)
    }

    #[tokio::test]
    async fn reopened_review_rechecks_existing_creation_and_mixed_candidate_bytes() {
        let (_root, _repository, run_dir, mut receipt, _) = fixture().await;
        assert!(super::super::validate_saved_review(&receipt, &run_dir).is_ok());
        std::fs::write(run_dir.join("candidate-1/code.py"), "value = 3\n").unwrap();
        assert!(super::super::validate_saved_review(&receipt, &run_dir).is_err());
        std::fs::write(run_dir.join("candidate-1/code.py"), "value = 2\n").unwrap();
        receipt.candidates[0].diff.push_str("stale diff");
        assert!(super::super::validate_saved_review(&receipt, &run_dir).is_err());

        let (_root, _repository, run_dir, receipt) = creation_fixture().await;
        assert!(super::super::validate_saved_review(&receipt, &run_dir).is_ok());
        std::fs::write(run_dir.join("candidate-1/src/new.py"), "changed\n").unwrap();
        assert!(super::super::validate_saved_review(&receipt, &run_dir).is_err());

        let (_root, _repository, run_dir, receipt) = mixed_fixture().await;
        assert!(super::super::validate_saved_review(&receipt, &run_dir).is_ok());
        std::fs::write(run_dir.join("baseline/code.py"), "changed\n").unwrap();
        assert!(super::super::validate_saved_review(&receipt, &run_dir).is_err());

        #[cfg(windows)]
        {
            let (_root, repository, run_dir, mut receipt) = creation_fixture().await;
            let files = inventory(&repository).await.unwrap();
            let path = PathBuf::from(r"src\new.py");
            let baseline = run_dir.join("baseline");
            let candidate_dir = run_dir.join("candidate-1");
            receipt.request.new_file = Some(path.clone());
            receipt.baseline_sha256 = scoped_hash(&baseline, &files, Some(&path), true).unwrap();
            receipt.candidates[0].content_sha256 =
                Some(scoped_hash(&candidate_dir, &files, Some(&path), true).unwrap());
            receipt.candidates[0].diff = baseline_diff_scoped(
                &baseline,
                &candidate_dir,
                &receipt.request.files,
                Some(&path),
            )
            .unwrap();
            assert!(super::super::validate_saved_review(&receipt, &run_dir).is_ok());
        }
    }

    #[tokio::test]
    async fn legacy_javascript_review_requires_new_source_inclusion_policy() {
        let (_root, _repository, run_dir, mut receipt, _) = fixture().await;
        receipt.schema = 2;
        receipt.request.files = vec!["code.js".into()];
        let candidate = &receipt.candidates[0];
        assert!(!reviewed_checks_match(&receipt, candidate));
        let error = super::super::validate_saved_review(&receipt, &run_dir).unwrap_err();
        assert!(error.to_string().contains("predates source-inclusion"));
    }

    #[tokio::test]
    async fn legacy_python_review_requires_new_source_inclusion_policy() {
        let (_root, _repository, run_dir, mut receipt, _) = fixture().await;
        receipt.schema = 3;
        let candidate = &receipt.candidates[0];
        assert!(!reviewed_checks_match(&receipt, candidate));
        let error = super::super::validate_saved_review(&receipt, &run_dir).unwrap_err();
        assert!(error.to_string().contains("predates source-inclusion"));
    }

    #[tokio::test]
    async fn rollback_restores_applied_existing_files_without_touching_dirty_work_or_index() {
        for batch in [false, true] {
            let (_root, repository, run_dir, receipt) = if batch {
                let (root, repository, run_dir, receipt) = two_file_fixture().await;
                (root, repository, run_dir, receipt)
            } else {
                let (root, repository, run_dir, receipt, _) = fixture().await;
                (root, repository, run_dir, receipt)
            };
            let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let index_path = &receipt.git_index.as_ref().unwrap().path;
            let original_index = std::fs::read(index_path).unwrap();
            apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            let restored = rollback_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            assert_eq!(restored.state, "rolled_back");
            assert_eq!(
                std::fs::read(repository.join("code.py")).unwrap(),
                b"value = 1\n"
            );
            assert_eq!(
                std::fs::read(repository.join("notes.md")).unwrap(),
                b"dirty worktree copy\n"
            );
            assert_eq!(
                std::fs::read(repository.join("loose.txt")).unwrap(),
                b"untracked copy\n"
            );
            assert_eq!(std::fs::read(index_path).unwrap(), original_index);
            assert_eq!(
                rollback_selected(&receipt, &run_dir, 1, selected)
                    .await
                    .unwrap()
                    .state,
                "rolled_back"
            );
            assert!(apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn rollback_refuses_later_source_edits_and_bad_backups_without_writing() {
        for corrupt_backup in [false, true] {
            let (_root, repository, run_dir, receipt) = two_file_fixture().await;
            let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let status = apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            if corrupt_backup {
                std::fs::write(run_dir.join(&status.files[0].backup), b"bad backup").unwrap();
            } else {
                std::fs::write(repository.join("notes.md"), b"later user edit\n").unwrap();
            }
            let code_before = std::fs::read(repository.join("code.py")).unwrap();
            let notes_before = std::fs::read(repository.join("notes.md")).unwrap();
            assert!(rollback_selected(&receipt, &run_dir, 1, selected)
                .await
                .is_err());
            assert_eq!(
                std::fs::read(repository.join("code.py")).unwrap(),
                code_before
            );
            assert_eq!(
                std::fs::read(repository.join("notes.md")).unwrap(),
                notes_before
            );
            assert_eq!(read_status(&run_dir).unwrap().unwrap().state, "applied");
        }
    }

    #[tokio::test]
    async fn rollback_resumes_each_known_partial_state_and_recreates_a_recorded_temporary() {
        for restored in 0..=2 {
            let (_root, repository, run_dir, receipt) = two_file_fixture().await;
            let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let mut status = apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            status.state = "rollback_prepared".into();
            status.rollback_temporaries = status
                .files
                .iter()
                .enumerate()
                .map(|(ordinal, file)| rollback_temporary(&file.path, &receipt.id, ordinal))
                .collect();
            save_status(&run_dir, &status).unwrap();
            if restored >= 1 {
                std::fs::write(repository.join("code.py"), b"value = 1\n").unwrap();
            }
            if restored >= 2 {
                std::fs::write(repository.join("notes.md"), b"dirty worktree copy\n").unwrap();
            } else {
                let temporary = repository.join(&status.rollback_temporaries[1]);
                std::fs::hard_link(run_dir.join(&status.files[1].backup), &temporary).unwrap();
            }
            let restored_status = rollback_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            assert_eq!(restored_status.state, "rolled_back");
            assert_eq!(
                std::fs::read(repository.join("code.py")).unwrap(),
                b"value = 1\n"
            );
            assert_eq!(
                std::fs::read(repository.join("notes.md")).unwrap(),
                b"dirty worktree copy\n"
            );
            assert!(!repository.join(&status.rollback_temporaries[1]).exists());
            assert_eq!(
                std::fs::read(run_dir.join(&status.files[1].backup)).unwrap(),
                b"dirty worktree copy\n"
            );
        }
    }

    #[tokio::test]
    async fn rollback_can_stop_a_prepared_forward_batch_with_a_recorded_candidate_temporary() {
        let (_root, repository, run_dir, receipt) = two_file_fixture().await;
        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let mut status = apply_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        status.state = "prepared".into();
        save_status(&run_dir, &status).unwrap();
        std::fs::write(repository.join("notes.md"), b"dirty worktree copy\n").unwrap();
        std::fs::write(
            repository.join(&status.files[1].temporary),
            b"candidate note\n",
        )
        .unwrap();
        let restored = rollback_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        assert_eq!(restored.state, "rolled_back");
        assert_eq!(
            std::fs::read(repository.join("code.py")).unwrap(),
            b"value = 1\n"
        );
        assert_eq!(
            std::fs::read(repository.join("notes.md")).unwrap(),
            b"dirty worktree copy\n"
        );
        assert!(!repository.join(&status.files[1].temporary).exists());
    }

    #[tokio::test]
    async fn partial_rollback_stops_when_a_remaining_file_has_later_edits() {
        let (_root, repository, run_dir, receipt) = two_file_fixture().await;
        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let mut status = apply_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        status.state = "rollback_prepared".into();
        status.rollback_temporaries = status
            .files
            .iter()
            .enumerate()
            .map(|(ordinal, file)| rollback_temporary(&file.path, &receipt.id, ordinal))
            .collect();
        save_status(&run_dir, &status).unwrap();
        std::fs::write(repository.join("code.py"), b"value = 1\n").unwrap();
        std::fs::write(repository.join("notes.md"), b"later user edit\n").unwrap();
        assert!(rollback_selected(&receipt, &run_dir, 1, selected)
            .await
            .is_err());
        assert_eq!(
            std::fs::read(repository.join("code.py")).unwrap(),
            b"value = 1\n"
        );
        assert_eq!(
            std::fs::read(repository.join("notes.md")).unwrap(),
            b"later user edit\n"
        );
        assert_eq!(
            read_status(&run_dir).unwrap().unwrap().state,
            "rollback_prepared"
        );
    }

    #[tokio::test]
    async fn rollback_refuses_changed_index_and_tampered_recovery_temporary() {
        for index_changed in [false, true] {
            let (_root, repository, run_dir, receipt) = two_file_fixture().await;
            let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let mut status = apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            if index_changed {
                git(&repository, &["add", "code.py"]);
            } else {
                status.state = "rollback_prepared".into();
                status.rollback_temporaries = status
                    .files
                    .iter()
                    .enumerate()
                    .map(|(ordinal, file)| rollback_temporary(&file.path, &receipt.id, ordinal))
                    .collect();
                save_status(&run_dir, &status).unwrap();
                std::fs::write(
                    repository.join(&status.rollback_temporaries[0]),
                    b"unknown\n",
                )
                .unwrap();
            }
            assert!(rollback_selected(&receipt, &run_dir, 1, selected)
                .await
                .is_err());
            assert_eq!(
                std::fs::read(repository.join("code.py")).unwrap(),
                b"value = 2\n"
            );
            assert_eq!(
                std::fs::read(repository.join("notes.md")).unwrap(),
                b"candidate note\n"
            );
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn rollback_created_file_preserves_index_and_unrelated_work_and_is_repeat_safe() {
        let (_root, repository, run_dir, receipt) = creation_fixture().await;
        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let applied = apply_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        let anchor = creation_anchor_path(
            &std::fs::canonicalize(&repository).unwrap(),
            &run_dir,
            applied.created_file.as_ref().unwrap(),
        )
        .unwrap();
        let index_path = receipt.git_index.as_ref().unwrap().path.clone();
        let index_before = std::fs::read(&index_path).unwrap();
        let restored = rollback_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        assert_eq!(restored.schema, 3);
        assert_eq!(restored.state, "rolled_back");
        assert!(restored.created_file.unwrap().rollback_deletion_attempted);
        assert!(!repository.join("src/new.py").exists());
        assert_eq!(std::fs::read(&anchor).unwrap(), b"value = 2\n");
        assert_eq!(std::fs::read(&index_path).unwrap(), index_before);
        assert_eq!(
            std::fs::read(repository.join("notes.md")).unwrap(),
            b"dirty worktree copy\n"
        );
        assert_eq!(
            std::fs::read(repository.join("loose.txt")).unwrap(),
            b"untracked copy\n"
        );
        assert_eq!(
            rollback_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap()
                .state,
            "rolled_back"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn rollback_created_file_refuses_replacement_mutation_or_missing_witness() {
        for change in [
            "identical replacement",
            "same identity edit",
            "missing anchor",
        ] {
            let (_root, repository, run_dir, receipt) = creation_fixture().await;
            let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let applied = apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            let target = repository.join("src/new.py");
            match change {
                "identical replacement" => {
                    std::fs::remove_file(&target).unwrap();
                    std::fs::write(&target, b"value = 2\n").unwrap();
                }
                "same identity edit" => std::fs::write(&target, b"value = 3\n").unwrap(),
                "missing anchor" => {
                    let anchor = creation_anchor_path(
                        &std::fs::canonicalize(&repository).unwrap(),
                        &run_dir,
                        applied.created_file.as_ref().unwrap(),
                    )
                    .unwrap();
                    std::fs::remove_file(anchor).unwrap();
                }
                _ => unreachable!(),
            }
            let before = std::fs::read(&target).unwrap();
            assert!(
                rollback_selected(&receipt, &run_dir, 1, selected)
                    .await
                    .is_err(),
                "{change}"
            );
            assert_eq!(std::fs::read(&target).unwrap(), before, "{change}");
            assert_eq!(read_status(&run_dir).unwrap().unwrap().state, "applied");
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn rollback_created_file_recovers_recorded_interruption_states() {
        for (attempted, remove_target) in [(false, false), (true, true)] {
            let (_root, repository, run_dir, receipt) = creation_fixture().await;
            let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let mut status = apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            status.state = "rollback_prepared".into();
            status
                .created_file
                .as_mut()
                .unwrap()
                .rollback_deletion_attempted = attempted;
            save_status(&run_dir, &status).unwrap();
            if remove_target {
                std::fs::remove_file(repository.join("src/new.py")).unwrap();
            }
            assert_eq!(
                rollback_selected(&receipt, &run_dir, 1, selected)
                    .await
                    .unwrap()
                    .state,
                "rolled_back"
            );
            assert!(!repository.join("src/new.py").exists());
        }
        let (_root, repository, run_dir, receipt) = creation_fixture().await;
        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let mut status = apply_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        status.state = "rollback_prepared".into();
        save_status(&run_dir, &status).unwrap();
        std::fs::remove_file(repository.join("src/new.py")).unwrap();
        assert!(rollback_selected(&receipt, &run_dir, 1, selected)
            .await
            .is_err());
        assert_eq!(
            read_status(&run_dir).unwrap().unwrap().state,
            "rollback_prepared"
        );

        let (_root, repository, run_dir, receipt) = creation_fixture().await;
        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let mut status = apply_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        status.state = "rollback_prepared".into();
        status
            .created_file
            .as_mut()
            .unwrap()
            .rollback_deletion_attempted = true;
        save_status(&run_dir, &status).unwrap();
        let target = repository.join("src/new.py");
        let anchor = creation_anchor_path(
            &std::fs::canonicalize(&repository).unwrap(),
            &run_dir,
            status.created_file.as_ref().unwrap(),
        )
        .unwrap();
        std::fs::remove_file(&target).unwrap();
        std::fs::hard_link(&anchor, &target).unwrap();
        assert!(rollback_selected(&receipt, &run_dir, 1, selected)
            .await
            .is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"value = 2\n");
        assert_eq!(
            read_status(&run_dir).unwrap().unwrap().state,
            "rollback_prepared"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn rollback_created_file_refuses_source_candidate_index_or_temporary_drift() {
        for change in ["source", "candidate", "index", "temporary"] {
            let (_root, repository, run_dir, receipt) = creation_fixture().await;
            let selected = receipt.candidates[0]
                .content_sha256
                .as_deref()
                .unwrap()
                .to_owned();
            let applied = apply_selected(&receipt, &run_dir, 1, &selected)
                .await
                .unwrap();
            match change {
                "source" => std::fs::write(repository.join("code.py"), b"value = 9\n").unwrap(),
                "candidate" => {
                    std::fs::write(run_dir.join("candidate-1/src/new.py"), b"value = 9\n").unwrap()
                }
                "index" => {
                    std::fs::write(&receipt.git_index.as_ref().unwrap().path, b"changed").unwrap()
                }
                "temporary" => std::fs::write(
                    repository.join(&applied.created_file.unwrap().temporary),
                    b"value = 2\n",
                )
                .unwrap(),
                _ => unreachable!(),
            }
            assert!(
                rollback_selected(&receipt, &run_dir, 1, &selected)
                    .await
                    .is_err(),
                "{change}"
            );
            assert_eq!(
                std::fs::read(repository.join("src/new.py")).unwrap(),
                b"value = 2\n"
            );
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn rollback_refuses_created_file_on_unverified_platform() {
        let (_root, repository, run_dir, receipt) = creation_fixture().await;
        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        apply_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        assert!(rollback_selected(&receipt, &run_dir, 1, selected)
            .await
            .is_err());
        assert_eq!(
            std::fs::read(repository.join("src/new.py")).unwrap(),
            b"value = 2\n"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn rollback_restores_mixed_creation_and_existing_edits() {
        let (_root, repository, run_dir, receipt) = mixed_fixture().await;
        let index_path = &receipt.git_index.as_ref().unwrap().path;
        let original_index = std::fs::read(index_path).unwrap();
        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        apply_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        let restored = rollback_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        assert_eq!(restored.schema, 4);
        assert_eq!(restored.state, "rolled_back");
        assert_eq!(
            std::fs::read(repository.join("code.py")).unwrap(),
            b"value = 1\n"
        );
        assert!(!repository.join("src/new.py").exists());
        assert_eq!(
            std::fs::read(repository.join("notes.md")).unwrap(),
            b"dirty worktree copy\n"
        );
        assert_eq!(std::fs::read(index_path).unwrap(), original_index);
        assert_eq!(
            rollback_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap()
                .state,
            "rolled_back"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn mixed_rollback_resumes_only_recorded_partial_states() {
        for interruption in [
            "before source restore",
            "source restored",
            "saved temporary",
            "deletion completed",
        ] {
            let (_root, repository, run_dir, receipt) = mixed_fixture().await;
            let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let mut status = apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            status.state = "rollback_prepared".into();
            status.rollback_temporaries = status
                .files
                .iter()
                .enumerate()
                .map(|(ordinal, file)| rollback_temporary(&file.path, &receipt.id, ordinal))
                .collect();
            match interruption {
                "source restored" => {
                    std::fs::write(repository.join("code.py"), b"value = 1\n").unwrap()
                }
                "saved temporary" => std::fs::write(
                    repository.join(&status.rollback_temporaries[0]),
                    b"value = 1\n",
                )
                .unwrap(),
                "deletion completed" => {
                    std::fs::write(repository.join("code.py"), b"value = 1\n").unwrap();
                    std::fs::remove_file(repository.join("src/new.py")).unwrap();
                    status
                        .created_file
                        .as_mut()
                        .unwrap()
                        .rollback_deletion_attempted = true;
                }
                _ => {}
            }
            save_status(&run_dir, &status).unwrap();
            let restored = rollback_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            assert_eq!(restored.state, "rolled_back", "{interruption}");
            assert!(restored.created_file.unwrap().rollback_deletion_attempted);
            assert_eq!(
                std::fs::read(repository.join("code.py")).unwrap(),
                b"value = 1\n"
            );
            assert!(!repository.join("src/new.py").exists());
            assert!(!repository.join(&status.rollback_temporaries[0]).exists());
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn mixed_rollback_accepts_created_path_ignored_by_reviewed_edit() {
        let (_root, repository, run_dir, mut receipt) = mixed_fixture().await;
        let created = receipt.request.new_file.clone().unwrap();
        std::fs::write(repository.join(".gitignore"), b"# baseline\n").unwrap();
        std::fs::write(run_dir.join("baseline/.gitignore"), b"# baseline\n").unwrap();
        std::fs::write(
            run_dir.join("candidate-1/.gitignore"),
            b"# baseline\nsrc/new.py\n",
        )
        .unwrap();
        receipt.request.files.push(".gitignore".into());
        receipt.request.editable_existing.push(".gitignore".into());
        let files = inventory(&repository).await.unwrap();
        receipt.baseline_sha256 =
            scoped_hash(&run_dir.join("baseline"), &files, Some(&created), true).unwrap();
        receipt.candidates[0].content_sha256 =
            Some(scoped_hash(&run_dir.join("candidate-1"), &files, Some(&created), true).unwrap());
        receipt.candidates[0].diff = baseline_diff_scoped(
            &run_dir.join("baseline"),
            &run_dir.join("candidate-1"),
            &receipt.request.files,
            Some(&created),
        )
        .unwrap();
        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let applied = apply_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        assert_eq!(applied.files.len(), 2);
        assert!(repository.join(&created).exists());
        assert_eq!(
            std::fs::read(repository.join(".gitignore")).unwrap(),
            b"# baseline\nsrc/new.py\n"
        );
        let restored = rollback_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        assert_eq!(restored.state, "rolled_back");
        assert_eq!(
            std::fs::read(repository.join(".gitignore")).unwrap(),
            b"# baseline\n"
        );
        assert_eq!(
            std::fs::read(repository.join("code.py")).unwrap(),
            b"value = 1\n"
        );
        assert!(!repository.join(&created).exists());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn mixed_publication_reports_the_creation_recheck_failure() {
        let (_root, repository, run_dir, receipt) = mixed_fixture().await;
        let repository = std::fs::canonicalize(repository).unwrap();
        let run_dir = std::fs::canonicalize(run_dir).unwrap();
        let path = receipt.request.new_file.as_deref().unwrap();
        let bytes = std::fs::read(run_dir.join("candidate-1").join(path)).unwrap();
        let created = creation_entry(path, &bytes, &receipt.id);
        std::fs::write(repository.join(".gitignore"), b"src/new.py\n").unwrap();

        let error = verify_unpublished_mixed_target(&repository, &run_dir, &created, path)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("creation path recheck failed"));
        assert!(error.to_string().contains("ignored by Git"));
        assert!(!repository.join(path).exists());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn mixed_rollback_refuses_drift_before_mutating_project() {
        for drift in [
            "created replacement",
            "missing target",
            "missing anchor",
            "source edit",
            "backup edit",
            "candidate edit",
            "index edit",
            "forward temporary",
            "creation temporary",
            "unrecorded rollback temporary",
        ] {
            let (_root, repository, run_dir, receipt) = mixed_fixture().await;
            let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let status = apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            let target = repository.join("src/new.py");
            match drift {
                "created replacement" => {
                    std::fs::remove_file(&target).unwrap();
                    std::fs::write(&target, b"value = 2\n").unwrap();
                }
                "missing target" => std::fs::remove_file(&target).unwrap(),
                "missing anchor" => {
                    let anchor = creation_anchor_path(
                        &std::fs::canonicalize(&repository).unwrap(),
                        &run_dir,
                        status.created_file.as_ref().unwrap(),
                    )
                    .unwrap();
                    std::fs::remove_file(anchor).unwrap();
                }
                "source edit" => {
                    std::fs::write(repository.join("code.py"), b"later user edit\n").unwrap()
                }
                "backup edit" => {
                    std::fs::write(run_dir.join(&status.files[0].backup), b"unknown backup\n")
                        .unwrap()
                }
                "candidate edit" => {
                    std::fs::write(run_dir.join("candidate-1/src/new.py"), b"value = 9\n").unwrap()
                }
                "index edit" => {
                    std::fs::write(&receipt.git_index.as_ref().unwrap().path, b"changed").unwrap()
                }
                "forward temporary" => {
                    std::fs::write(repository.join(&status.files[0].temporary), b"value = 3\n")
                        .unwrap()
                }
                "creation temporary" => std::fs::write(
                    repository.join(&status.created_file.as_ref().unwrap().temporary),
                    b"value = 2\n",
                )
                .unwrap(),
                "unrecorded rollback temporary" => {
                    std::fs::write(
                        repository.join(rollback_temporary(&status.files[0].path, &receipt.id, 0)),
                        b"value = 1\n",
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }
            let source_before = std::fs::read(repository.join("code.py")).unwrap();
            let target_before = std::fs::read(&target).ok();
            assert!(
                rollback_selected(&receipt, &run_dir, 1, selected)
                    .await
                    .is_err(),
                "{drift}"
            );
            assert_eq!(
                std::fs::read(repository.join("code.py")).unwrap(),
                source_before,
                "{drift}"
            );
            assert_eq!(std::fs::read(&target).ok(), target_before, "{drift}");
            assert_eq!(
                read_status(&run_dir).unwrap().unwrap().state,
                "applied",
                "{drift}"
            );
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn mixed_rollback_refuses_ambiguous_deletion_markers() {
        for attempted in [false, true] {
            let (_root, repository, run_dir, receipt) = mixed_fixture().await;
            let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let mut status = apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            status.state = "rollback_prepared".into();
            status.rollback_temporaries = status
                .files
                .iter()
                .enumerate()
                .map(|(ordinal, file)| rollback_temporary(&file.path, &receipt.id, ordinal))
                .collect();
            status
                .created_file
                .as_mut()
                .unwrap()
                .rollback_deletion_attempted = attempted;
            if attempted {
                std::fs::write(repository.join("code.py"), b"value = 1\n").unwrap();
            } else {
                std::fs::remove_file(repository.join("src/new.py")).unwrap();
            }
            save_status(&run_dir, &status).unwrap();
            assert!(rollback_selected(&receipt, &run_dir, 1, selected)
                .await
                .is_err());
            assert_eq!(
                read_status(&run_dir).unwrap().unwrap().state,
                "rollback_prepared"
            );
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn rollback_refuses_mixed_creation_without_platform_identity_proof() {
        let (_root, repository, run_dir, receipt) = mixed_fixture().await;
        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        apply_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        assert!(rollback_selected(&receipt, &run_dir, 1, selected)
            .await
            .is_err());
        assert!(repository.join("src/new.py").exists());
    }

    #[tokio::test]
    async fn byte_identical_replacement_cannot_be_claimed_as_applied_creation() {
        for mixed in [false, true] {
            let (_root, repository, run_dir, receipt) = if mixed {
                mixed_fixture().await
            } else {
                creation_fixture().await
            };
            let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let status = apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            let target = repository.join("src/new.py");
            std::fs::remove_file(&target).unwrap();
            std::fs::write(&target, b"value = 2\n").unwrap();
            assert!(apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .is_err());
            assert_eq!(std::fs::read(&target).unwrap(), b"value = 2\n");
            assert_eq!(read_status(&run_dir).unwrap().unwrap().state, status.state);
        }
    }

    #[tokio::test]
    async fn creation_recovery_refuses_unwitnessed_identical_bytes() {
        for target_present in [false, true] {
            let (_root, repository, run_dir, receipt) = creation_fixture().await;
            let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let entry = creation_entry(Path::new("src/new.py"), b"value = 2\n", &receipt.id);
            let status = LocalApplyReceipt {
                schema: 3,
                run_id: receipt.id.clone(),
                candidate_number: 1,
                path: None,
                before_sha256: None,
                after_sha256: None,
                files: vec![],
                created_file: Some(entry.clone()),
                baseline_sha256: Some(receipt.baseline_sha256.clone()),
                candidate_sha256: Some(selected.into()),
                state: "prepared".into(),
                rollback_temporaries: Vec::new(),
                detail: "legacy prepared fixture".into(),
            };
            save_status(&run_dir, &status).unwrap();
            let path = if target_present {
                repository.join(&entry.path)
            } else {
                repository.join(&entry.temporary)
            };
            std::fs::write(&path, b"value = 2\n").unwrap();
            assert!(apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .is_err());
            assert_eq!(std::fs::read(&path).unwrap(), b"value = 2\n");
            assert_eq!(read_status(&run_dir).unwrap().unwrap().state, "prepared");
        }
    }

    #[tokio::test]
    async fn legacy_creation_journal_cannot_republish_a_deleted_file() {
        for mixed in [false, true] {
            let (_root, repository, run_dir, receipt) = if mixed {
                mixed_fixture().await
            } else {
                creation_fixture().await
            };
            let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let mut status = apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            std::fs::remove_file(repository.join("src/new.py")).unwrap();
            let created = status.created_file.as_mut().unwrap();
            std::fs::remove_file(creation_anchor_path(&repository, &run_dir, created).unwrap())
                .unwrap();
            created.anchor = None;
            created.identity = None;
            created.publication_attempted = false;
            status.state = "prepared".into();
            save_status(&run_dir, &status).unwrap();
            assert!(apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .is_err());
            assert!(!repository.join("src/new.py").exists());
            assert_eq!(read_status(&run_dir).unwrap().unwrap().state, "prepared");
        }
    }

    #[cfg(windows)]
    #[test]
    fn disposition_unlinks_only_the_validated_created_name() {
        let root = tempfile::tempdir().unwrap();
        let anchor = root.path().join("anchor.bin");
        let target = root.path().join("created.py");
        let moved = root.path().join("moved.py");
        std::fs::write(&anchor, b"value = 2\n").unwrap();
        std::fs::hard_link(&anchor, &target).unwrap();
        let mut entry = creation_entry(Path::new("created.py"), b"value = 2\n", "fixture");
        let original_identity = observe_creation_file(&anchor).unwrap().0;
        entry.identity = Some(original_identity.clone());
        let (target_file, anchor_file) =
            open_created_deletion_handles(&target, &anchor, &entry).unwrap();
        assert!(std::fs::rename(&target, &moved).is_err());
        dispose_created_target(target_file).unwrap();
        drop(anchor_file);
        assert!(!target.exists());
        assert!(!moved.exists());
        assert_eq!(std::fs::read(&anchor).unwrap(), b"value = 2\n");
        assert_eq!(observe_creation_file(&anchor).unwrap().0, original_identity);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn cross_volume_creation_uses_local_git_identity_anchor_when_requested() {
        let Some(other_volume) = std::env::var_os("PHONTON_CROSS_VOLUME_TEST_DIR") else {
            return;
        };
        let (_root, _original_repository, run_dir, mut receipt) = creation_fixture().await;
        let other_volume = PathBuf::from(other_volume);
        assert_ne!(
            directory_device(&other_volume).unwrap(),
            directory_device(&run_dir).unwrap(),
            "test directory must be on another volume"
        );
        let second_root = tempfile::Builder::new()
            .prefix("phonton-cross-volume-")
            .tempdir_in(&other_volume)
            .unwrap();
        let repository = second_root.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        git(&repository, &["init", "--quiet"]);
        std::fs::write(repository.join("code.py"), "value = 1\n").unwrap();
        std::fs::write(repository.join("notes.md"), "staged copy\n").unwrap();
        git(&repository, &["add", "code.py", "notes.md"]);
        std::fs::write(repository.join("notes.md"), "dirty worktree copy\n").unwrap();
        std::fs::write(repository.join("loose.txt"), "untracked copy\n").unwrap();
        std::fs::create_dir(repository.join("src")).unwrap();
        receipt.request.repository = repository.clone();
        let mut index = integrity::capture(&repository).await.unwrap();
        assert!(integrity::verify(&repository, &mut index, "final review").await);
        receipt.git_index = Some(index);

        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let applied = apply_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        let created = applied.created_file.as_ref().unwrap();
        let canonical_repository = std::fs::canonicalize(&repository).unwrap();
        assert_eq!(applied.state, "applied");
        assert_eq!(
            created.anchor.as_deref(),
            Some(
                git_creation_anchor(&canonical_repository, &run_dir)
                    .unwrap()
                    .as_path()
            )
        );
        assert_eq!(
            std::fs::read(repository.join("src/new.py")).unwrap(),
            b"value = 2\n"
        );
        assert!(
            creation_anchor_path(&canonical_repository, &run_dir, created)
                .unwrap()
                .exists()
        );
        std::fs::remove_file(repository.join("src/new.py")).unwrap();
        std::fs::write(repository.join("src/new.py"), b"value = 2\n").unwrap();
        assert!(apply_selected(&receipt, &run_dir, 1, selected)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn creation_recovery_refuses_missing_identity_anchor_and_missing_target() {
        for remove_anchor in [false, true] {
            let (_root, repository, run_dir, receipt) = creation_fixture().await;
            let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let mut status = apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            status.state = "prepared".into();
            save_status(&run_dir, &status).unwrap();
            if remove_anchor {
                std::fs::remove_file(run_dir.join("apply-created-anchor.bin")).unwrap();
            } else {
                std::fs::remove_file(repository.join("src/new.py")).unwrap();
            }
            assert!(apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .is_err());
            assert_eq!(read_status(&run_dir).unwrap().unwrap().state, "prepared");
        }
    }

    #[tokio::test]
    async fn legacy_one_file_unrecorded_forward_temporary_needs_manual_inspection() {
        let (_root, repository, run_dir, receipt, _) = fixture().await;
        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let mut status = apply_one(&receipt, &run_dir, 1, selected).await.unwrap();
        status.state = "prepared".into();
        save_status(&run_dir, &status).unwrap();
        std::fs::write(repository.join("code.py"), b"value = 1\n").unwrap();
        let mut unknown = tempfile::NamedTempFile::new_in(&repository).unwrap();
        unknown.write_all(b"value = 2\n").unwrap();
        assert!(rollback_selected(&receipt, &run_dir, 1, selected)
            .await
            .is_err());
        assert_eq!(
            std::fs::read(repository.join("code.py")).unwrap(),
            b"value = 1\n"
        );
        assert!(unknown.path().exists());
        unknown.close().unwrap();
        assert_eq!(
            rollback_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap()
                .state,
            "rolled_back"
        );
    }

    #[tokio::test]
    async fn mixed_apply_publishes_new_file_and_replaces_reviewed_caller() {
        let (_root, repository, run_dir, receipt) = mixed_fixture().await;
        let index_before = std::fs::read(receipt.git_index.as_ref().unwrap().path.clone()).unwrap();
        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let status = apply_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        assert_eq!(status.schema, 4);
        assert_eq!(status.state, "applied");
        assert_eq!(status.files.len(), 1);
        assert_eq!(
            std::fs::read(repository.join("code.py")).unwrap(),
            b"value = 3\n"
        );
        assert_eq!(
            std::fs::read(repository.join("src/new.py")).unwrap(),
            b"value = 2\n"
        );
        assert_eq!(
            std::fs::read(repository.join("notes.md")).unwrap(),
            b"dirty worktree copy\n"
        );
        assert_eq!(
            std::fs::read(receipt.git_index.as_ref().unwrap().path.clone()).unwrap(),
            index_before
        );
        assert_eq!(
            apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap()
                .state,
            "applied"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn mixed_apply_handles_existing_edit_under_a_separate_parent() {
        let (_root, repository, run_dir, mut receipt) = mixed_fixture().await;
        let source = PathBuf::from("other/caller.py");
        let baseline = run_dir.join("baseline");
        let candidate_dir = run_dir.join("candidate-1");
        for directory in [&repository, &baseline, &candidate_dir] {
            std::fs::create_dir(directory.join("other")).unwrap();
            std::fs::rename(directory.join("code.py"), directory.join(&source)).unwrap();
        }
        git(&repository, &["add", "-A"]);
        let files = inventory(&repository).await.unwrap();
        let created = receipt.request.new_file.as_ref().unwrap();
        receipt.request.files = vec![source.clone()];
        receipt.request.editable_existing = vec![source.clone()];
        receipt.baseline_sha256 = scoped_hash(&baseline, &files, Some(created), true).unwrap();
        receipt.candidates[0].content_sha256 =
            Some(scoped_hash(&candidate_dir, &files, Some(created), true).unwrap());
        receipt.candidates[0].diff = baseline_diff_scoped(
            &baseline,
            &candidate_dir,
            &receipt.request.files,
            Some(created),
        )
        .unwrap();
        let mut index = integrity::capture(&repository).await.unwrap();
        assert!(integrity::verify(&repository, &mut index, "final review").await);
        receipt.git_index = Some(index);
        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let status = apply_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        assert_eq!(status.state, "applied");
        assert_eq!(
            std::fs::read(repository.join(&source)).unwrap(),
            b"value = 3\n"
        );
        assert_eq!(
            std::fs::read(repository.join(created)).unwrap(),
            b"value = 2\n"
        );
    }

    #[tokio::test]
    async fn mixed_apply_accepts_passing_preparation_before_verification() {
        let (_root, repository, run_dir, mut receipt) = mixed_fixture().await;
        let setup = LocalCheck {
            program: "npm".into(),
            args: vec!["ci".into(), "--offline".into()],
        };
        receipt.request.preparation = Some(setup.clone());
        receipt.candidates[0].checks.insert(
            0,
            CheckEvidence {
                check: Some(setup),
                purpose: CheckPurpose::Preparation,
                status: CheckStatus::Passed,
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                detail: "offline dependencies prepared".into(),
                elapsed_ms: 0,
            },
        );
        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        assert_eq!(
            apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap()
                .state,
            "applied"
        );
        assert_eq!(
            std::fs::read(repository.join("src/new.py")).unwrap(),
            b"value = 2\n"
        );
    }

    #[tokio::test]
    async fn mixed_apply_refuses_missing_or_failed_preparation() {
        let (_root, _repository, run_dir, mut receipt) = mixed_fixture().await;
        let setup = LocalCheck {
            program: "npm".into(),
            args: vec!["ci".into(), "--offline".into()],
        };
        receipt.request.preparation = Some(setup.clone());
        let selected = receipt.candidates[0].content_sha256.clone().unwrap();
        assert!(apply_selected(&receipt, &run_dir, 1, &selected)
            .await
            .is_err());
        receipt.candidates[0].checks.insert(
            0,
            CheckEvidence {
                check: Some(setup),
                purpose: CheckPurpose::Preparation,
                status: CheckStatus::Failed,
                exit_code: Some(1),
                stdout: String::new(),
                stderr: String::new(),
                detail: "offline setup failed".into(),
                elapsed_ms: 0,
            },
        );
        assert!(apply_selected(&receipt, &run_dir, 1, &selected)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn mixed_apply_recovers_each_recorded_partial_state() {
        for (created, edited) in [(false, false), (true, false), (false, true), (true, true)] {
            let (_root, repository, run_dir, receipt) = mixed_fixture().await;
            let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let mut status = apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            status.state = "prepared".into();
            save_status(&run_dir, &status).unwrap();
            if !created {
                std::fs::remove_file(repository.join("src/new.py")).unwrap();
            }
            if !edited {
                std::fs::write(repository.join("code.py"), "value = 1\n").unwrap();
            }
            if !created {
                assert!(apply_selected(&receipt, &run_dir, 1, selected)
                    .await
                    .is_err());
                assert!(!repository.join("src/new.py").exists());
                assert_eq!(
                    std::fs::read(repository.join("code.py")).unwrap(),
                    if edited {
                        b"value = 3\n"
                    } else {
                        b"value = 1\n"
                    }
                );
                continue;
            }
            let resumed = apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            assert_eq!(
                resumed.state, "applied",
                "created={created}, edited={edited}"
            );
            assert_eq!(
                std::fs::read(repository.join("src/new.py")).unwrap(),
                b"value = 2\n"
            );
            assert_eq!(
                std::fs::read(repository.join("code.py")).unwrap(),
                b"value = 3\n"
            );
        }
    }

    #[tokio::test]
    async fn mixed_apply_refuses_incomplete_or_unknown_states() {
        let (_root, repository, run_dir, mut receipt) = mixed_fixture().await;
        receipt.candidates[0].stage = CandidateStage::CreationPendingEdit;
        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        assert!(apply_selected(&receipt, &run_dir, 1, selected)
            .await
            .is_err());
        assert!(!repository.join("src/new.py").exists());
        assert_eq!(
            std::fs::read(repository.join("code.py")).unwrap(),
            b"value = 1\n"
        );

        for drift in ["source", "target", "index", "backup", "temporary"] {
            let (_root, repository, run_dir, receipt) = mixed_fixture().await;
            let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let mut status = apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            status.state = "prepared".into();
            save_status(&run_dir, &status).unwrap();
            std::fs::write(repository.join("code.py"), "value = 1\n").unwrap();
            match drift {
                "source" => {
                    std::fs::write(repository.join("code.py"), "third-party edit\n").unwrap()
                }
                "target" => {
                    std::fs::write(repository.join("src/new.py"), "third-party file\n").unwrap()
                }
                "index" => git(&repository, &["add", "loose.txt"]),
                "backup" => {
                    std::fs::write(run_dir.join(&status.files[0].backup), "wrong backup\n").unwrap()
                }
                _ => std::fs::write(repository.join(&status.files[0].temporary), "wrong temp\n")
                    .unwrap(),
            }
            assert!(
                apply_selected(&receipt, &run_dir, 1, selected)
                    .await
                    .is_err(),
                "{drift}"
            );
            assert_eq!(
                std::fs::read(repository.join("code.py")).unwrap(),
                if drift == "source" {
                    b"third-party edit\n".as_slice()
                } else {
                    b"value = 1\n".as_slice()
                },
                "{drift}"
            );
        }
    }

    #[tokio::test]
    async fn creation_apply_publishes_once_without_replacing_or_touching_existing_source() {
        let (_root, repository, run_dir, receipt) = creation_fixture().await;
        let index_before = std::fs::read(receipt.git_index.as_ref().unwrap().path.clone()).unwrap();
        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let status = apply_selected(&receipt, &run_dir, 1, selected)
            .await
            .unwrap();
        assert_eq!(status.schema, 3);
        assert_eq!(status.state, "applied");
        assert_eq!(
            std::fs::read(repository.join("src/new.py")).unwrap(),
            b"value = 2\n"
        );
        assert_eq!(
            std::fs::read(repository.join("code.py")).unwrap(),
            b"value = 1\n"
        );
        assert_eq!(
            std::fs::read(repository.join("notes.md")).unwrap(),
            b"dirty worktree copy\n"
        );
        assert_eq!(
            std::fs::read(receipt.git_index.as_ref().unwrap().path.clone()).unwrap(),
            index_before
        );
        assert_eq!(
            apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap()
                .state,
            "applied"
        );

        let (_root, repository, run_dir, receipt) = creation_fixture().await;
        std::fs::write(repository.join("src/new.py"), "someone else's file\n").unwrap();
        let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
        assert!(apply_selected(&receipt, &run_dir, 1, selected)
            .await
            .is_err());
        assert_eq!(
            std::fs::read(repository.join("src/new.py")).unwrap(),
            b"someone else's file\n"
        );
        assert!(!run_dir.join("apply.json").exists());
    }

    #[tokio::test]
    async fn prepared_creation_recovers_before_temp_and_after_publication() {
        for stage in 0..3 {
            let (_root, repository, run_dir, receipt) = creation_fixture().await;
            let path = PathBuf::from("src/new.py");
            let selected = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let mut entry = creation_entry(&path, b"value = 2\n", &receipt.id);
            let mut status = LocalApplyReceipt {
                schema: 3,
                run_id: receipt.id.clone(),
                candidate_number: 1,
                path: None,
                before_sha256: None,
                after_sha256: None,
                files: vec![],
                created_file: Some(entry.clone()),
                baseline_sha256: Some(receipt.baseline_sha256.clone()),
                candidate_sha256: Some(selected.into()),
                state: "prepared".into(),
                rollback_temporaries: Vec::new(),
                detail: "interrupted fixture".into(),
            };
            if stage >= 1 {
                std::fs::write(repository.join(&entry.temporary), "value = 2\n").unwrap();
                std::fs::hard_link(
                    repository.join(&entry.temporary),
                    run_dir.join("apply-created-anchor.bin"),
                )
                .unwrap();
                entry.identity = Some(
                    observe_creation_file(&repository.join(&entry.temporary))
                        .unwrap()
                        .0,
                );
            }
            if stage == 2 {
                entry.publication_attempted = true;
                std::fs::hard_link(repository.join(&entry.temporary), repository.join(&path))
                    .unwrap();
            }
            status.created_file = Some(entry.clone());
            save_status(&run_dir, &status).unwrap();
            let resumed = apply_selected(&receipt, &run_dir, 1, selected)
                .await
                .unwrap();
            assert_eq!(resumed.state, "applied", "stage {stage}");
            assert_eq!(
                std::fs::read(repository.join(&path)).unwrap(),
                b"value = 2\n"
            );
            assert!(!repository.join(&entry.temporary).exists());
        }
    }

    #[tokio::test]
    async fn creation_recovery_refuses_unknown_temp_target_and_changed_candidate() {
        let (_root, repository, run_dir, receipt) = creation_fixture().await;
        let selected = receipt.candidates[0].content_sha256.clone().unwrap();
        std::fs::write(run_dir.join("candidate-1/src/new.py"), "tampered\n").unwrap();
        assert!(apply_selected(&receipt, &run_dir, 1, &selected)
            .await
            .is_err());
        assert!(!repository.join("src/new.py").exists());
        std::fs::write(run_dir.join("candidate-1/src/new.py"), "value = 2\n").unwrap();
        let entry = creation_entry(Path::new("src/new.py"), b"value = 2\n", &receipt.id);
        let status = LocalApplyReceipt {
            schema: 3,
            run_id: receipt.id.clone(),
            candidate_number: 1,
            path: None,
            before_sha256: None,
            after_sha256: None,
            files: vec![],
            created_file: Some(entry.clone()),
            baseline_sha256: Some(receipt.baseline_sha256.clone()),
            candidate_sha256: Some(selected.clone()),
            state: "prepared".into(),
            rollback_temporaries: Vec::new(),
            detail: "interrupted fixture".into(),
        };
        save_status(&run_dir, &status).unwrap();
        std::fs::write(repository.join(&entry.temporary), "unknown temporary\n").unwrap();
        assert!(apply_selected(&receipt, &run_dir, 1, &selected)
            .await
            .is_err());
        assert!(!repository.join("src/new.py").exists());
        std::fs::remove_file(repository.join(&entry.temporary)).unwrap();
        std::fs::write(repository.join("src/new.py"), "someone else's file\n").unwrap();
        assert!(apply_selected(&receipt, &run_dir, 1, &selected)
            .await
            .is_err());
        assert_eq!(
            std::fs::read(repository.join("src/new.py")).unwrap(),
            b"someone else's file\n"
        );
    }

    #[cfg(windows)]
    #[test]
    fn held_creation_parent_cannot_be_renamed_during_publication() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("src");
        std::fs::create_dir(&parent).unwrap();
        let held = hold_apply_parent(&parent).unwrap();
        assert!(std::fs::rename(&parent, root.path().join("moved")).is_err());
        drop(held);
        std::fs::rename(&parent, root.path().join("moved")).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn held_existing_source_parents_cannot_be_renamed_during_apply() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        std::fs::create_dir_all(repository.join("src/nested")).unwrap();
        std::fs::create_dir(repository.join("other")).unwrap();
        let paths = [
            PathBuf::from("src/nested/code.py"),
            PathBuf::from("other/caller.py"),
        ];
        let held = hold_source_parents(&repository, paths.iter().map(PathBuf::as_path)).unwrap();
        for (name, moved) in [
            ("src/nested", "moved-nested"),
            ("src", "moved-src"),
            ("other", "moved-other"),
        ] {
            assert!(std::fs::rename(repository.join(name), root.path().join(moved)).is_err());
        }
        drop(held);
        for (name, moved) in [("src", "moved-src"), ("other", "moved-other")] {
            std::fs::rename(repository.join(name), root.path().join(moved)).unwrap();
        }
    }

    #[tokio::test]
    async fn applies_complete_verified_batch_without_touching_index_or_unscoped_file() {
        let (_root, repository, run_dir, receipt) = two_file_fixture().await;
        let index_before = std::fs::read(receipt.git_index.as_ref().unwrap().path.clone()).unwrap();
        let selected_hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let status = apply_selected(&receipt, &run_dir, 1, selected_hash)
            .await
            .unwrap();
        assert_eq!(status.schema, 2);
        assert_eq!(status.state, "applied");
        assert_eq!(status.files.len(), 2);
        assert_eq!(
            std::fs::read(repository.join("code.py")).unwrap(),
            b"value = 2\n"
        );
        assert_eq!(
            std::fs::read(repository.join("notes.md")).unwrap(),
            b"candidate note\n"
        );
        assert_eq!(
            std::fs::read(repository.join("loose.txt")).unwrap(),
            b"untracked copy\n"
        );
        assert_eq!(
            std::fs::read(receipt.git_index.as_ref().unwrap().path.clone()).unwrap(),
            index_before
        );
        assert_eq!(
            std::fs::read(run_dir.join(&status.files[0].backup)).unwrap(),
            b"value = 1\n"
        );
        assert_eq!(
            std::fs::read(run_dir.join(&status.files[1].backup)).unwrap(),
            b"dirty worktree copy\n"
        );
        assert_eq!(
            apply_selected(&receipt, &run_dir, 1, selected_hash)
                .await
                .unwrap()
                .state,
            "applied"
        );
    }

    #[tokio::test]
    async fn prepared_batch_recovers_before_partial_and_after_all_replacements() {
        for recovered in [0, 1, 2] {
            let (_root, repository, run_dir, receipt) = two_file_fixture().await;
            let selected_hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let mut status = apply_selected(&receipt, &run_dir, 1, selected_hash)
                .await
                .unwrap();
            status.state = "prepared".into();
            save_status(&run_dir, &status).unwrap();
            if recovered < 1 {
                std::fs::write(repository.join("code.py"), "value = 1\n").unwrap();
            }
            if recovered < 2 {
                std::fs::write(repository.join("notes.md"), "dirty worktree copy\n").unwrap();
            }
            let resumed = apply_selected(&receipt, &run_dir, 1, selected_hash)
                .await
                .unwrap();
            assert_eq!(resumed.state, "applied", "recovered {recovered}");
            assert_eq!(
                std::fs::read(repository.join("code.py")).unwrap(),
                b"value = 2\n"
            );
            assert_eq!(
                std::fs::read(repository.join("notes.md")).unwrap(),
                b"candidate note\n"
            );
        }
    }

    #[tokio::test]
    async fn recorded_candidate_temporary_can_finish_an_interrupted_replacement() {
        let (_root, repository, run_dir, receipt) = two_file_fixture().await;
        let selected_hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let mut status = apply_selected(&receipt, &run_dir, 1, selected_hash)
            .await
            .unwrap();
        status.state = "prepared".into();
        save_status(&run_dir, &status).unwrap();
        std::fs::write(repository.join("notes.md"), "dirty worktree copy\n").unwrap();
        let temporary = repository.join(&status.files[1].temporary);
        std::fs::write(&temporary, "candidate note\n").unwrap();

        let resumed = apply_selected(&receipt, &run_dir, 1, selected_hash)
            .await
            .unwrap();
        assert_eq!(resumed.state, "applied");
        assert!(!temporary.exists());
        assert_eq!(
            std::fs::read(repository.join("notes.md")).unwrap(),
            b"candidate note\n"
        );
    }

    #[tokio::test]
    async fn altered_recorded_temporary_blocks_resume_without_writing() {
        let (_root, repository, run_dir, receipt) = two_file_fixture().await;
        let selected_hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let mut status = apply_selected(&receipt, &run_dir, 1, selected_hash)
            .await
            .unwrap();
        status.state = "prepared".into();
        save_status(&run_dir, &status).unwrap();
        std::fs::write(repository.join("notes.md"), "dirty worktree copy\n").unwrap();
        let temporary = repository.join(&status.files[1].temporary);
        std::fs::write(&temporary, "unknown temporary bytes\n").unwrap();

        assert!(apply_selected(&receipt, &run_dir, 1, selected_hash)
            .await
            .is_err());
        assert_eq!(
            std::fs::read(repository.join("notes.md")).unwrap(),
            b"dirty worktree copy\n"
        );
        assert_eq!(
            std::fs::read(&temporary).unwrap(),
            b"unknown temporary bytes\n"
        );
    }

    #[tokio::test]
    async fn recovered_hardlinked_temporary_is_unlinked_before_replacement() {
        let (root, repository, run_dir, receipt) = two_file_fixture().await;
        let selected_hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let mut status = apply_selected(&receipt, &run_dir, 1, selected_hash)
            .await
            .unwrap();
        status.state = "prepared".into();
        save_status(&run_dir, &status).unwrap();
        std::fs::write(repository.join("notes.md"), "dirty worktree copy\n").unwrap();
        let external = root.path().join("external-candidate.txt");
        std::fs::write(&external, "candidate note\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&external, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let temporary = repository.join(&status.files[1].temporary);
        std::fs::hard_link(&external, &temporary).unwrap();

        let resumed = apply_selected(&receipt, &run_dir, 1, selected_hash)
            .await
            .unwrap();
        assert_eq!(resumed.state, "applied");
        assert_eq!(std::fs::read(&external).unwrap(), b"candidate note\n");
        assert!(!temporary.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&external).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn recovery_inventory_detects_new_untracked_source() {
        let (_root, repository, run_dir, receipt) = two_file_fixture().await;
        let selected_hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let status = apply_selected(&receipt, &run_dir, 1, selected_hash)
            .await
            .unwrap();
        let reviewed = inventory(&repository).await.unwrap();
        std::fs::write(repository.join("new-source.txt"), "late source\n").unwrap();
        assert_ne!(
            checked_batch_inventory(&repository, &status.files)
                .await
                .unwrap(),
            reviewed
        );
    }

    #[tokio::test]
    async fn unknown_partial_bytes_or_index_drift_refuse_batch_resume() {
        for drift in ["source", "index", "backup"] {
            let (_root, repository, run_dir, receipt) = two_file_fixture().await;
            let selected_hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
            let mut status = apply_selected(&receipt, &run_dir, 1, selected_hash)
                .await
                .unwrap();
            status.state = "prepared".into();
            save_status(&run_dir, &status).unwrap();
            std::fs::write(repository.join("notes.md"), "dirty worktree copy\n").unwrap();
            match drift {
                "source" => {
                    std::fs::write(repository.join("code.py"), "third-party edit\n").unwrap()
                }
                "index" => git(&repository, &["add", "loose.txt"]),
                _ => {
                    std::fs::write(run_dir.join(&status.files[1].backup), "wrong backup\n").unwrap()
                }
            }
            let before = std::fs::read(repository.join("notes.md")).unwrap();
            assert!(
                apply_selected(&receipt, &run_dir, 1, selected_hash)
                    .await
                    .is_err(),
                "{drift}"
            );
            assert_eq!(
                std::fs::read(repository.join("notes.md")).unwrap(),
                before,
                "{drift}"
            );
        }
    }

    #[tokio::test]
    async fn unrecorded_temporary_refuses_batch_and_schema_one_remains_readable() {
        let (_root, repository, run_dir, receipt) = two_file_fixture().await;
        let selected_hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let temporary = repository.join(
            batch_file(
                "code.py".into(),
                b"value = 1\n",
                b"value = 2\n",
                &receipt.id,
                0,
            )
            .temporary,
        );
        std::fs::write(&temporary, "surprise\n").unwrap();
        assert!(apply_selected(&receipt, &run_dir, 1, selected_hash)
            .await
            .is_err());
        assert_eq!(
            std::fs::read(repository.join("code.py")).unwrap(),
            b"value = 1\n"
        );
        assert!(!run_dir.join("apply.json").exists());

        let (_root, _repository, run_dir, receipt, _) = fixture().await;
        let selected_hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let old = apply_one(&receipt, &run_dir, 1, selected_hash)
            .await
            .unwrap();
        assert_eq!(old.schema, 1);
        let reread = read_status(&run_dir).unwrap().unwrap();
        assert_eq!(reread.path, Some("code.py".into()));
        assert_eq!(
            apply_selected(&receipt, &run_dir, 1, selected_hash)
                .await
                .unwrap()
                .schema,
            1
        );
    }

    #[tokio::test]
    async fn applies_one_verified_file_without_touching_dirty_work_or_index() {
        let (_root, repository, run_dir, receipt, _files) = fixture().await;
        let index_before = std::fs::read(receipt.git_index.as_ref().unwrap().path.clone()).unwrap();
        let hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let applied = apply_one(&receipt, &run_dir, 1, hash).await.unwrap();
        assert_eq!(applied.state, "applied");
        assert_eq!(
            std::fs::read(repository.join("code.py")).unwrap(),
            b"value = 2\n"
        );
        assert_eq!(
            std::fs::read(repository.join("notes.md")).unwrap(),
            b"dirty worktree copy\n"
        );
        assert_eq!(
            std::fs::read(repository.join("loose.txt")).unwrap(),
            b"untracked copy\n"
        );
        assert_eq!(
            std::fs::read(receipt.git_index.as_ref().unwrap().path.clone()).unwrap(),
            index_before
        );
        assert_eq!(
            std::fs::read(run_dir.join("apply-backup.bin")).unwrap(),
            b"value = 1\n"
        );
        assert_eq!(
            apply_one(&receipt, &run_dir, 1, hash).await.unwrap().state,
            "applied"
        );
    }

    #[tokio::test]
    async fn failed_prepared_journal_sync_does_not_replace_source_or_index() {
        fn refuse_status(run_dir: &Path, status: &LocalApplyReceipt) -> Result<()> {
            super::super::persist_json_with_sync(
                &run_dir.join("apply.json"),
                &serde_json::to_value(status)?,
                |_| Err(invalid("injected journal directory sync failure")),
            )
        }
        let (_root, repository, run_dir, receipt, _files) = fixture().await;
        let source = repository.join("code.py");
        let source_before = std::fs::read(&source).unwrap();
        let index = &receipt.git_index.as_ref().unwrap().path;
        let index_before = std::fs::read(index).unwrap();
        let hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let error = apply_one_with_status_writer(
            &receipt,
            &run_dir,
            1,
            hash,
            refuse_status,
            sync_source_parent,
        )
        .await
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("injected journal directory sync failure"));
        assert_eq!(std::fs::read(&source).unwrap(), source_before);
        assert_eq!(std::fs::read(index).unwrap(), index_before);
        assert_eq!(
            std::fs::read(run_dir.join("apply-backup.bin")).unwrap(),
            source_before
        );
        assert_eq!(read_status(&run_dir).unwrap().unwrap().state, "prepared");
    }

    #[tokio::test]
    async fn failed_source_parent_sync_keeps_prepared_journal_for_recovery() {
        fn refuse_source_sync(source: &Path) -> Result<()> {
            assert_eq!(std::fs::read(source)?, b"value = 2\n");
            Err(invalid("injected source parent sync failure"))
        }
        let (_root, repository, run_dir, receipt, _files) = fixture().await;
        let source = repository.join("code.py");
        let index = &receipt.git_index.as_ref().unwrap().path;
        let index_before = std::fs::read(index).unwrap();
        let hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
        let error = apply_one_with_status_writer(
            &receipt,
            &run_dir,
            1,
            hash,
            save_status,
            refuse_source_sync,
        )
        .await
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("injected source parent sync failure"));
        assert_eq!(std::fs::read(&source).unwrap(), b"value = 2\n");
        assert_eq!(std::fs::read(index).unwrap(), index_before);
        assert_eq!(read_status(&run_dir).unwrap().unwrap().state, "prepared");

        let recovered = apply_one(&receipt, &run_dir, 1, hash).await.unwrap();
        assert_eq!(recovered.state, "applied");
        assert_eq!(std::fs::read(&source).unwrap(), b"value = 2\n");
        assert_eq!(std::fs::read(index).unwrap(), index_before);
    }

    #[test]
    fn new_evidence_publication_never_replaces_an_existing_name() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("apply-backup.bin");
        std::fs::write(&path, b"existing backup").unwrap();
        assert!(write_new_evidence(&path, b"replacement").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing backup");
    }

    #[tokio::test]
    async fn schema_one_replay_refuses_missing_or_corrupt_original_backup() {
        for journal_state in ["prepared", "applied"] {
            for backup_state in ["missing", "corrupt"] {
                let (_root, repository, run_dir, receipt, _files) = fixture().await;
                let selected_hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
                let mut status = apply_one(&receipt, &run_dir, 1, selected_hash)
                    .await
                    .unwrap();
                status.state = journal_state.into();
                save_status(&run_dir, &status).unwrap();
                let backup = run_dir.join("apply-backup.bin");
                if backup_state == "missing" {
                    std::fs::remove_file(&backup).unwrap();
                } else {
                    std::fs::write(&backup, b"wrong original\n").unwrap();
                }
                let journal_before = std::fs::read(run_dir.join("apply.json")).unwrap();
                let source_before = std::fs::read(repository.join("code.py")).unwrap();
                let index_path = &receipt.git_index.as_ref().unwrap().path;
                let index_before = std::fs::read(index_path).unwrap();
                assert!(
                    apply_one(&receipt, &run_dir, 1, selected_hash)
                        .await
                        .is_err(),
                    "{journal_state}/{backup_state}"
                );
                assert_eq!(
                    std::fs::read(run_dir.join("apply.json")).unwrap(),
                    journal_before
                );
                assert_eq!(
                    std::fs::read(repository.join("code.py")).unwrap(),
                    source_before
                );
                assert_eq!(std::fs::read(index_path).unwrap(), index_before);
            }
        }
    }

    #[tokio::test]
    async fn stale_source_index_or_candidate_refuses_without_a_project_write() {
        for altered in ["source", "index", "candidate"] {
            let (_root, repository, run_dir, receipt, _files) = fixture().await;
            match altered {
                "source" => std::fs::write(repository.join("code.py"), "user edit\n").unwrap(),
                "index" => git(&repository, &["add", "loose.txt"]),
                _ => std::fs::write(run_dir.join("candidate-1/code.py"), "tampered\n").unwrap(),
            }
            let before = std::fs::read(repository.join("code.py")).unwrap();
            let hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
            assert!(
                apply_one(&receipt, &run_dir, 1, hash).await.is_err(),
                "{altered}"
            );
            assert_eq!(
                std::fs::read(repository.join("code.py")).unwrap(),
                before,
                "{altered}"
            );
            assert!(!run_dir.join("apply.json").exists(), "{altered}");
        }
    }

    #[tokio::test]
    async fn incomplete_or_mismatched_checks_cannot_be_applied() {
        for altered in ["missing", "different", "extra"] {
            let (_root, repository, run_dir, mut receipt, _files) = fixture().await;
            match altered {
                "missing" => receipt.candidates[0].checks.clear(),
                "different" => {
                    receipt.candidates[0].checks[0].check.as_mut().unwrap().args =
                        vec!["another.py".into()]
                }
                _ => receipt.request.checks.push(LocalCheck {
                    program: "python".into(),
                    args: vec!["second.py".into()],
                }),
            }
            let hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
            assert!(
                apply_one(&receipt, &run_dir, 1, hash).await.is_err(),
                "{altered}"
            );
            assert_eq!(
                std::fs::read(repository.join("code.py")).unwrap(),
                b"value = 1\n"
            );
            assert!(!run_dir.join("apply.json").exists());
        }
    }

    #[tokio::test]
    async fn journal_ignores_the_old_predictable_temp_path() {
        let (_root, repository, run_dir, receipt, _files) = fixture().await;
        let old_temp = run_dir.join("apply.json.tmp");
        std::fs::hard_link(repository.join("code.py"), &old_temp).unwrap();
        let hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
        assert_eq!(
            apply_one(&receipt, &run_dir, 1, hash).await.unwrap().state,
            "applied"
        );
        assert_eq!(std::fs::read(old_temp).unwrap(), b"value = 1\n");
        assert_eq!(
            std::fs::read(repository.join("code.py")).unwrap(),
            b"value = 2\n"
        );
    }

    #[tokio::test]
    async fn linked_apply_journal_is_not_read_or_replaced() {
        let (_root, repository, run_dir, receipt, _files) = fixture().await;
        if !file_symlink(&repository.join("code.py"), &run_dir.join("apply.json")) {
            return;
        }
        assert!(read_status(&run_dir).is_err());
        let hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
        assert!(apply_one(&receipt, &run_dir, 1, hash).await.is_err());
        assert_eq!(
            std::fs::read(repository.join("code.py")).unwrap(),
            b"value = 1\n"
        );
    }

    #[tokio::test]
    async fn linked_backup_or_temp_cannot_replace_source_or_backup() {
        let (_root, repository, run_dir, receipt, _files) = fixture().await;
        if !file_symlink(
            &repository.join("code.py"),
            &run_dir.join("apply-backup.bin"),
        ) {
            return;
        }
        let hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
        assert!(apply_one(&receipt, &run_dir, 1, hash).await.is_err());
        assert_eq!(
            std::fs::read(repository.join("code.py")).unwrap(),
            b"value = 1\n"
        );
        assert!(!run_dir.join("apply.json").exists());

        let (_root, repository, run_dir, receipt, _files) = fixture().await;
        std::fs::write(
            repository.join(".git/info/exclude"),
            ".code.py.phonton-apply-*.tmp\n",
        )
        .unwrap();
        let legacy = repository.join(format!(".code.py.phonton-apply-{}.tmp", receipt.id));
        if !file_symlink(&run_dir.join("candidate-1/code.py"), &legacy) {
            return;
        }
        let hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
        assert_eq!(
            apply_one(&receipt, &run_dir, 1, hash).await.unwrap().state,
            "applied"
        );
        assert!(std::fs::symlink_metadata(repository.join("code.py"))
            .unwrap()
            .file_type()
            .is_file());
        assert!(std::fs::symlink_metadata(legacy)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preserves_executable_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (_root, repository, run_dir, receipt, _files) = fixture().await;
        std::fs::set_permissions(
            repository.join("code.py"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let hash = receipt.candidates[0].content_sha256.as_deref().unwrap();
        apply_one(&receipt, &run_dir, 1, hash).await.unwrap();
        let mode = std::fs::metadata(repository.join("code.py"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }
}
