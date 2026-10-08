//! Observe original index identity without staging, refreshing or restoring it.
use super::{invalid, Result};
use phonton_types::{local::CheckStatus, local_run::GitIndexEvidence};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path, process::Stdio, time::Duration};

async fn snapshot(root: &Path) -> Result<(std::path::PathBuf, Option<String>)> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(["-c", "core.fsmonitor=false", "-C"])
        .arg(root)
        .args(["rev-parse", "--path-format=absolute", "--git-path", "index"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Observe this repository, not a caller's alternate staging area.
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
    ] {
        command.env_remove(name);
    }
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let output = tokio::time::timeout(Duration::from_secs(10), async {
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| invalid("Missing Git index path output"))?;
        let output = super::read_inventory(stdout).await?;
        if !child.wait().await?.success() {
            return Err(invalid("Cannot resolve the original Git index"));
        }
        Ok::<_, super::RunError>(output)
    })
    .await
    .map_err(|_| invalid("Git index lookup timed out"))??;
    let path = std::str::from_utf8(&output)
        .map_err(|_| invalid("Non-UTF8 Git index path"))?
        .trim_end_matches(['\r', '\n']);
    let path = std::path::PathBuf::from(path);
    if !path.is_absolute() || path.as_os_str().is_empty() {
        return Err(invalid("Git did not return an absolute index path"));
    }
    Ok((path.clone(), hash_index(&path)?))
}

fn hash_index(path: &Path) -> Result<Option<String>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    const LIMIT: usize = 32 * 1024 * 1024;
    if !file.metadata()?.is_file() || file.metadata()?.len() > LIMIT as u64 {
        return Err(invalid(
            "Git index is not a regular file within the 32 MiB observation limit",
        ));
    }
    let mut bytes = 0usize;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n;
        if bytes > LIMIT {
            return Err(invalid(
                "Git index exceeded its observation limit during reading",
            ));
        }
        hash.update(&buffer[..n]);
    }
    Ok(Some(format!("{:x}", hash.finalize())))
}

pub(super) async fn capture(root: &Path) -> Result<GitIndexEvidence> {
    let (path, hash) = snapshot(root).await?;
    Ok(GitIndexEvidence {
        path,
        before_sha256: hash,
        after_sha256: None,
        status: CheckStatus::NotRun,
        stage: "before checks".into(),
        detail: "Original Git index captured; no post-check comparison has run.".into(),
    })
}

pub(super) async fn verify(root: &Path, evidence: &mut GitIndexEvidence, stage: &str) -> bool {
    evidence.stage = stage.into();
    match snapshot(root).await {
        Ok((path, hash)) => {
            let same = path == evidence.path && hash == evidence.before_sha256;
            evidence.after_sha256 = hash;
            evidence.status = if same {
                CheckStatus::Passed
            } else {
                CheckStatus::Failed
            };
            evidence.detail = if same {
                "Original Git index path and bytes match the captured state at this observation. This is not containment.".into()
            } else {
                "Original Git index path or bytes changed. No candidate can be selected; the index was not restored automatically.".into()
            };
            same
        }
        Err(error) => {
            evidence.after_sha256 = None;
            evidence.status = CheckStatus::Unavailable;
            evidence.detail = format!("Original Git index could not be rechecked: {error}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[tokio::test]
    async fn detects_creation_change_and_deletion_without_restoring_index() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "--quiet"]);
        let mut absent = capture(root.path()).await.unwrap();
        assert!(absent.before_sha256.is_none());
        assert!(verify(root.path(), &mut absent, "unchanged").await);
        std::fs::write(root.path().join("source.js"), "original\n").unwrap();
        git(root.path(), &["add", "source.js"]);
        assert!(!verify(root.path(), &mut absent, "index created").await);
        let mut staged = capture(root.path()).await.unwrap();
        let original = std::fs::read(&staged.path).unwrap();
        assert!(verify(root.path(), &mut staged, "unchanged").await);
        std::fs::write(root.path().join("source.js"), "changed\n").unwrap();
        git(root.path(), &["add", "source.js"]);
        assert!(!verify(root.path(), &mut staged, "index changed").await);
        assert_eq!(staged.status, CheckStatus::Failed);
        assert_ne!(std::fs::read(&staged.path).unwrap(), original);
        std::fs::remove_file(&staged.path).unwrap();
        assert!(!verify(root.path(), &mut staged, "index removed").await);
        assert!(!staged.path.exists());
    }

    #[tokio::test]
    async fn resolves_git_file_indirection_and_unavailable_observation() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let metadata = root.path().join("metadata");
        git(
            &work,
            &[
                "init",
                "--quiet",
                "--separate-git-dir",
                metadata.to_str().unwrap(),
            ],
        );
        assert!(work.join(".git").is_file());
        let mut evidence = capture(&work).await.unwrap();
        assert_eq!(
            std::fs::canonicalize(evidence.path.parent().unwrap()).unwrap(),
            std::fs::canonicalize(&metadata).unwrap()
        );
        assert_eq!(evidence.path.file_name().unwrap(), "index");
        std::fs::create_dir(&evidence.path).unwrap();
        assert!(!verify(&work, &mut evidence, "unreadable index").await);
        assert_eq!(evidence.status, CheckStatus::Unavailable);
    }

    #[tokio::test]
    async fn failed_observation_clears_selection_and_cannot_be_erased_by_restoring_bytes() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "--quiet"]);
        std::fs::write(root.path().join("code.js"), "before\n").unwrap();
        git(root.path(), &["add", "code.js"]);
        let evidence = capture(root.path()).await.unwrap();
        let original = std::fs::read(&evidence.path).unwrap();
        let mut receipt: phonton_types::local_run::LocalRunReceipt = serde_json::from_value(serde_json::json!({
            "schema":1,"id":"index-regression","state":"review_ready",
            "request":{"goal":"fixture","repository":root.path(),"files":["code.js"]},
            "profile":{"schema":1,"model":"fixture","digest":"fixture","runtime_version":"fixture",
                "endpoint":"http://127.0.0.1:11434","context_tokens":4096,"output_tokens":512,
                "protocol":null,"probes":[],"hardware":phonton_types::local::HardwareSnapshot::default(),"measured_at_unix":0},
            "hardware":phonton_types::local::HardwareSnapshot::default(),"baseline_sha256":"fixture",
            "baseline_checks":[],"candidates":[],"selected_candidate":1,"checks_used":0,
            "generated_tokens_reserved":0,"elapsed_ms":0,"known_gaps":[],"git_index":evidence
        })).unwrap();
        std::fs::write(root.path().join("code.js"), "after\n").unwrap();
        git(root.path(), &["add", "code.js"]);
        assert!(
            !super::super::verify_original_index(root.path(), &mut receipt, "final review").await
        );
        assert_eq!(receipt.selected_candidate, None);
        assert_eq!(receipt.state, "git_index_changed_or_unavailable");
        let failed_hash = receipt.git_index.as_ref().unwrap().after_sha256.clone();
        std::fs::write(&receipt.git_index.as_ref().unwrap().path, original).unwrap();
        assert!(!super::super::verify_original_index(root.path(), &mut receipt, "later").await);
        assert_eq!(
            receipt.git_index.as_ref().unwrap().status,
            CheckStatus::Failed
        );
        assert_eq!(
            receipt.git_index.as_ref().unwrap().after_sha256,
            failed_hash
        );
    }
}
