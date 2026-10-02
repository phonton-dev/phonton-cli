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

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Filename inside `~/.phonton/`.
const TRUST_FILENAME: &str = "trusted_workspaces.json";

#[derive(Debug, Serialize, Deserialize)]
struct TrustFile {
    /// Schema version. Currently `1`.
    #[serde(default = "default_version")]
    version: u32,
    /// Canonicalised absolute paths the user has trusted.
    #[serde(default)]
    trusted: Vec<String>,
}

impl Default for TrustFile {
    fn default() -> Self {
        Self {
            version: default_version(),
            trusted: Vec::new(),
        }
    }
}

fn default_version() -> u32 {
    1
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

/// Has the user previously trusted this workspace?
pub fn is_trusted(workspace: &Path) -> bool {
    let key = canonical_key(workspace);
    load().trusted.iter().any(|p| p == &key)
}

/// Persist a trust decision for `workspace`. Idempotent.
pub fn record_trust(workspace: &Path) -> Result<()> {
    let key = canonical_key(workspace);
    let mut file = load();
    if !file.trusted.iter().any(|p| p == &key) {
        file.trusted.push(key);
    }
    save(&file)
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
        let _ = record_trust(workspace);
        return Ok(true);
    }

    let abs = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let folder = display_path(&abs);
    let color =
        std::io::IsTerminal::is_terminal(&io::stdout()) && std::env::var_os("NO_COLOR").is_none();
    let paint = |code: &str, text: &str| {
        if color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    };
    let text = [
        String::new(),
        format!(
            "{}  {}",
            paint("1;38;2;236;235;230", "phonton"),
            paint("38;2;163;161;154", "· first run in this folder")
        ),
        String::new(),
        paint("1;38;2;236;235;230", &short(&folder, 64)),
        String::new(),
        paint(
            "38;2;163;161;154",
            "Phonton reads source here, proposes diffs and runs",
        ),
        paint(
            "38;2;163;161;154",
            "this project's own checks. Changes land only when",
        ),
        paint("38;2;163;161;154", "you accept a review."),
        String::new(),
        paint(
            "38;2;102;100;94",
            "Local model: nothing leaves this machine.",
        ),
        paint(
            "38;2;102;100;94",
            "Cloud provider: the source a plan selects is sent.",
        ),
        paint(
            "38;2;102;100;94",
            "~/.ssh, ~/.aws and similar paths stay blocked.",
        ),
        String::new(),
        paint(
            "38;2;102;100;94",
            "Remembered in ~/.phonton/trusted_workspaces.json",
        ),
    ];
    let phi = if color {
        crate::art::ansi(&crate::art::phi(None))
    } else {
        Vec::new()
    };
    println!();
    for row in 0..phi.len().max(text.len()) {
        let left = phi.get(row).cloned().unwrap_or_else(|| {
            if color {
                " ".repeat(crate::art::PHI_WIDTH as usize)
            } else {
                String::new()
            }
        });
        let right = text.get(row).map(String::as_str).unwrap_or("");
        let gap = if color { "    " } else { "  " };
        println!("  {left}{gap}{right}");
    }
    println!();
    print!(
        "  {} {} ",
        paint("1;38;2;95;224;232", "›"),
        paint(
            "1;38;2;236;235;230",
            "Trust this folder and start Phonton? [y/N]"
        )
    );
    io::stdout().flush().ok();

    let mut buf = String::new();
    io::stdin().read_line(&mut buf).ok();
    let answer = buf.trim().to_ascii_lowercase();
    if answer == "y" || answer == "yes" {
        match record_trust(workspace) {
            Ok(()) => println!("  Trusted. Launching…\n"),
            Err(e) => {
                println!("Trust accepted (but persistence failed: {e}) — launching Phonton…\n")
            }
        }
        Ok(true)
    } else {
        println!("  Not trusted. Nothing was read. Run phonton from a folder you want to work in.");
        Ok(false)
    }
}

/// Windows canonical paths carry a `\\?\` verbatim prefix; users never type it.
fn display_path(path: &Path) -> String {
    let raw = path.display().to_string();
    raw.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(raw)
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
    fn display_path_drops_the_verbatim_prefix() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Users\me\repo")),
            r"C:\Users\me\repo"
        );
        assert_eq!(display_path(Path::new("/home/me/repo")), "/home/me/repo");
    }

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
        assert_eq!(de.version, 1);
    }

    #[test]
    fn forward_compat_unknown_fields_ignored() {
        let raw = r#"{ "version": 2, "trusted": ["a"], "future": "x" }"#;
        let de: TrustFile = serde_json::from_str(raw).unwrap_or_default();
        assert_eq!(de.trusted, vec!["a"]);
    }
}
