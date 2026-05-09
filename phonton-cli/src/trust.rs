//! Workspace-trust gate.
//!
//! Before the TUI starts dispatching workers that can read source, write
//! files, run `cargo`, and reach LLM APIs, the user must explicitly
//! consent to giving Phonton those rights *for this folder*. This is the
//! same UX an IDE shows when you open a project for the first time.
//!
//! The decision is persisted in `~/.phonton/trusted_workspaces.json` so
//! the prompt fires once per workspace, not every launch. Users can
//! revoke trust by deleting the entry from that file.
//!
//! ## Storage shape
//!
//! ```json
//! {
//!   "version": 1,
//!   "trusted": [
//!     "/Users/me/code/some-project",
//!     "C:\\Users\\me\\code\\other"
//!   ]
//! }
//! ```
//!
//! ## Why this lives here, not in `phonton-sandbox`
//!
//! `phonton-sandbox::ExecutionGuard` gates *individual tool calls* at
//! execution time (block writes to `~/.ssh/`, etc.). Workspace trust is
//! a different layer — it asks the user once, up front, "do you want
//! Phonton to operate on this folder at all?" Like VS Code's
//! `security.workspace.trust`. The two layers complement each other:
//! trusting a workspace doesn't relax the per-call guard.
//!
//! ## Failure mode
//!
//! If the file can't be read or parsed we return `false` (untrusted) and
//! the prompt fires. If it can't be written, the user sees a warning
//! after consenting but the run proceeds — losing only the persistence,
//! not the trust decision for this session.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use phonton_types::{PermissionMode, WorkspaceTrustRecord, WorkspaceTrustSource};
use serde::{Deserialize, Serialize};

/// Filename inside `~/.phonton/`.
const TRUST_FILENAME: &str = "trusted_workspaces.json";

#[derive(Debug, Serialize, Deserialize)]
struct TrustFile {
    /// Schema version. Currently `2`.
    #[serde(default = "default_version")]
    version: u32,
    /// Canonicalised absolute paths the user has trusted.
    #[serde(default)]
    trusted: Vec<String>,
    /// Structured per-workspace trust metadata.
    #[serde(default)]
    records: Vec<WorkspaceTrustRecord>,
}

impl Default for TrustFile {
    fn default() -> Self {
        Self {
            version: default_version(),
            trusted: Vec::new(),
            records: Vec::new(),
        }
    }
}

fn default_version() -> u32 {
    2
}

fn trust_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".phonton").join(TRUST_FILENAME))
}

fn load() -> TrustFile {
    let Some(p) = trust_path() else {
        return TrustFile::default();
    };
    let Ok(raw) = std::fs::read_to_string(&p) else {
        return TrustFile::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save(file: &TrustFile) -> Result<()> {
    let p = trust_path().context("could not determine ~/.phonton path")?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let raw = serde_json::to_string_pretty(file)?;
    std::fs::write(&p, raw)?;
    Ok(())
}

fn canonical_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Return structured trust metadata for `workspace`, including legacy entries.
pub fn trust_record(workspace: &Path) -> Option<WorkspaceTrustRecord> {
    let key = canonical_key(workspace);
    let file = load();
    if let Some(record) = file
        .records
        .iter()
        .find(|record| record.canonical_path == key)
    {
        return Some(record.clone());
    }
    if file.trusted.iter().any(|p| p == &key) {
        let now = now_secs();
        return Some(WorkspaceTrustRecord {
            canonical_path: key,
            display_name: display_name(workspace),
            trusted_at: now,
            last_seen_at: now,
            permission_mode: PermissionMode::Ask,
            source: WorkspaceTrustSource::LegacyJson,
        });
    }
    None
}

/// List all known trust records, converting legacy path entries on read.
pub fn list_trust_records() -> Vec<WorkspaceTrustRecord> {
    let file = load();
    let mut records = file.records;
    for path in file.trusted {
        if records
            .iter()
            .any(|record| record.canonical_path.eq_ignore_ascii_case(&path))
        {
            continue;
        }
        records.push(WorkspaceTrustRecord {
            canonical_path: path.clone(),
            display_name: Path::new(&path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(path.as_str())
                .to_string(),
            trusted_at: 0,
            last_seen_at: 0,
            permission_mode: PermissionMode::Ask,
            source: WorkspaceTrustSource::LegacyJson,
        });
    }
    records.sort_by_key(|record| std::cmp::Reverse(record.last_seen_at));
    records
}

/// Has the user previously trusted this workspace?
pub fn is_trusted(workspace: &Path) -> bool {
    trust_record(workspace).is_some()
}

/// Persist a trust decision for `workspace`. Idempotent.
pub fn record_trust(workspace: &Path) -> Result<()> {
    record_trust_with_mode(
        workspace,
        PermissionMode::Ask,
        WorkspaceTrustSource::JsonRecord,
    )
}

/// Persist structured trust metadata for `workspace`.
pub fn record_trust_with_mode(
    workspace: &Path,
    permission_mode: PermissionMode,
    source: WorkspaceTrustSource,
) -> Result<()> {
    let key = canonical_key(workspace);
    let mut file = load();
    let now = now_secs();
    let trusted_at = file
        .records
        .iter()
        .find(|record| record.canonical_path == key)
        .map(|record| record.trusted_at)
        .unwrap_or(now);
    if !file.trusted.iter().any(|p| p == &key) {
        file.trusted.push(key.clone());
    }
    file.records.retain(|record| record.canonical_path != key);
    file.records.push(WorkspaceTrustRecord {
        canonical_path: key,
        display_name: display_name(workspace),
        trusted_at,
        last_seen_at: now,
        permission_mode,
        source,
    });
    save(&file)
}

/// Revoke persisted trust for one workspace.
pub fn revoke_trust(workspace: &Path) -> Result<bool> {
    let key = canonical_key(workspace);
    let mut file = load();
    let before_paths = file.trusted.len();
    let before_records = file.records.len();
    file.trusted.retain(|path| path != &key);
    file.records.retain(|record| record.canonical_path != key);
    save(&file)?;
    Ok(before_paths != file.trusted.len() || before_records != file.records.len())
}

/// Show a blocking trust prompt on stdout/stdin and return `true` if the
/// user accepted. Used **before** the TUI's alternate-screen mode is
/// entered so the message is visible in the user's normal terminal.
///
/// Behaviour:
/// * Already trusted → returns `true` without prompting.
/// * `PHONTON_TRUST_ALL=1` env var → returns `true` without prompting
///   (CI / scripted use).
/// * Trusted on accept → persists to `~/.phonton/trusted_workspaces.json`
///   and returns `true`.
/// * Declined → returns `false`; caller must exit.
pub fn prompt_if_needed(workspace: &Path) -> Result<bool> {
    if is_trusted(workspace) {
        return Ok(true);
    }
    if std::env::var("PHONTON_TRUST_ALL")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        let _ = record_trust_with_mode(
            workspace,
            PermissionMode::Ask,
            WorkspaceTrustSource::EnvOverride,
        );
        return Ok(true);
    }

    let abs = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    println!();
    println!("┌──────────────────────────────────────────────────────────────────┐");
    println!("│  Phonton — workspace trust                                       │");
    println!("├──────────────────────────────────────────────────────────────────┤");
    println!("│  Phonton can read files, write changes, run `cargo`, and call    │");
    println!("│  external LLM APIs in this folder. Tool calls are still gated    │");
    println!("│  per-action (.ssh/, .aws/, etc. are blocked outright), but you   │");
    println!("│  should only trust folders whose source you intend to edit.      │");
    println!("│                                                                  │");
    println!("│  Folder:                                                         │");
    println!("│    {:<62}│", short(&abs.display().to_string(), 62));
    println!("│                                                                  │");
    println!("│  This decision is remembered in:                                 │");
    println!("│    ~/.phonton/trusted_workspaces.json                            │");
    println!("└──────────────────────────────────────────────────────────────────┘");
    print!("Trust this folder and start Phonton? [y/N]: ");
    io::stdout().flush().ok();

    let mut buf = String::new();
    io::stdin().read_line(&mut buf).ok();
    let answer = buf.trim().to_ascii_lowercase();
    if answer == "y" || answer == "yes" {
        match record_trust(workspace) {
            Ok(()) => println!("Trust recorded — launching Phonton…\n"),
            Err(e) => {
                println!("Trust accepted (but persistence failed: {e}) — launching Phonton…\n")
            }
        }
        Ok(true)
    } else {
        println!("Trust declined — exiting. Re-run from a folder you want to work in.");
        Ok(false)
    }
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max - 3).collect();
    format!("{head}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_workspace_is_not_trusted() {
        // Tempdir guaranteed not in user's trust file.
        let td = tempfile::tempdir().unwrap();
        assert!(!is_trusted(td.path()));
    }

    #[test]
    fn record_then_load_round_trip_via_in_memory_file() {
        let mut f = TrustFile::default();
        f.trusted.push("/tmp/some/path".into());
        let ser = serde_json::to_string(&f).unwrap();
        let de: TrustFile = serde_json::from_str(&ser).unwrap();
        assert_eq!(de.trusted, vec!["/tmp/some/path"]);
        assert_eq!(de.version, 2);
    }

    #[test]
    fn forward_compat_unknown_fields_ignored() {
        let raw = r#"{ "version": 2, "trusted": ["a"], "future": "x" }"#;
        let de: TrustFile = serde_json::from_str(raw).unwrap_or_default();
        assert_eq!(de.trusted, vec!["a"]);
    }

    #[test]
    fn structured_records_survive_json_round_trip() {
        let raw = r#"{
            "version": 2,
            "trusted": ["C:\\work\\repo"],
            "records": [{
                "canonical_path": "C:\\work\\repo",
                "display_name": "repo",
                "trusted_at": 10,
                "last_seen_at": 20,
                "permission_mode": "workspace-write",
                "source": "json-record"
            }]
        }"#;

        let de: TrustFile = serde_json::from_str(raw).unwrap();

        assert_eq!(de.records.len(), 1);
        assert_eq!(
            de.records[0].permission_mode,
            PermissionMode::WorkspaceWrite
        );
    }
}
