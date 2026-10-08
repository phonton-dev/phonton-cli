//! The run record: numbers only receipts can move.
//!
//! A verified run extends the streak; an unverified or failed run ends it.
//! Tokens are split by where the model ran. Stored next to the local model
//! state (`~/.phonton/record.json`, or beside `PHONTON_LOCAL_STATE`).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    pub runs: u64,
    pub verified_runs: u64,
    pub streak: u64,
    pub best_streak: u64,
    /// Tokens processed by a model on this machine.
    pub local_tokens: u64,
    /// Tokens sent to a hosted provider.
    pub cloud_tokens: u64,
    /// Tokens whose inference location was not established.
    pub unknown_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Verified,
    Unverified,
    Failed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TokenOrigin {
    ManagedLocal,
    Hosted,
    #[default]
    Unknown,
}

impl From<phonton_types::local_run::RuntimeOrigin> for TokenOrigin {
    fn from(origin: phonton_types::local_run::RuntimeOrigin) -> Self {
        match origin {
            phonton_types::local_run::RuntimeOrigin::ManagedVerified => Self::ManagedLocal,
            _ => Self::Unknown,
        }
    }
}

impl Record {
    pub fn add(&mut self, outcome: Outcome, tokens: u64, origin: TokenOrigin) {
        self.runs += 1;
        match origin {
            TokenOrigin::ManagedLocal => {
                self.local_tokens = self.local_tokens.saturating_add(tokens)
            }
            TokenOrigin::Hosted => self.cloud_tokens = self.cloud_tokens.saturating_add(tokens),
            TokenOrigin::Unknown => {
                self.unknown_tokens = self.unknown_tokens.saturating_add(tokens)
            }
        }
        if outcome == Outcome::Verified {
            self.verified_runs += 1;
            self.streak += 1;
            self.best_streak = self.best_streak.max(self.streak);
        } else {
            self.streak = 0;
        }
    }
}

fn path() -> Option<PathBuf> {
    Some(
        crate::models_cli::state_path()
            .ok()?
            .parent()?
            .join("record.json"),
    )
}

/// The saved record; missing or unreadable files start from zero.
pub fn load() -> Record {
    path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Add one finished run and save. Returns only the durable updated record.
pub fn add_run(outcome: Outcome, tokens: u64, origin: TokenOrigin) -> Record {
    match try_add_run(outcome, tokens, origin) {
        Ok(record) => record,
        Err(error) => {
            eprintln!("Run receipt retained, but the aggregate record could not be saved: {error}");
            load()
        }
    }
}

fn try_add_run(outcome: Outcome, tokens: u64, origin: TokenOrigin) -> std::io::Result<Record> {
    let path = path().ok_or_else(|| std::io::Error::other("run record path unavailable"))?;
    add_run_at(&path, outcome, tokens, origin)
}

fn add_run_at(
    path: &std::path::Path,
    outcome: Outcome,
    tokens: u64,
    origin: TokenOrigin,
) -> std::io::Result<Record> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // All CLI/Desktop writers share this lock. Drop releases it after atomic replacement.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    lock.lock()?;
    let mut record: Record = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Record::default(),
        Err(error) => return Err(error),
    };
    record.add(outcome, tokens, origin);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(&record).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(tmp, path)?;
    Ok(record)
}

/// Count a finished local-harness run (desktop or `phonton goal --local`).
pub fn add_local_receipt(receipt: &phonton_types::local_run::LocalRunReceipt) -> Record {
    match record_local_receipt(receipt) {
        Ok(Some(record)) => record,
        Ok(None) => load(),
        Err(error) => {
            eprintln!("Run receipt retained, but the aggregate record could not be saved: {error}");
            load()
        }
    }
}

/// None means no model attempt qualified; success means the record is durable.
pub fn record_local_receipt(
    receipt: &phonton_types::local_run::LocalRunReceipt,
) -> std::io::Result<Option<Record>> {
    if receipt.candidates.is_empty() && receipt.hypotheses.is_empty() {
        // Stopped before any model attempt (admission, baseline).
        return Ok(None);
    }
    let (input, output) = crate::local_tui::tokens(receipt);
    let tokens = input.saturating_add(output);
    let outcome = match receipt.state.as_str() {
        "review_ready" => Outcome::Verified,
        "review_unverified" => Outcome::Unverified,
        _ => Outcome::Failed,
    };
    try_add_run(outcome, tokens, receipt.runtime_origin.into()).map(Some)
}

/// `phonton record [--json]`.
pub fn run(args: &[String]) -> anyhow::Result<()> {
    let r = load();
    if args.iter().any(|a| a == "--json") {
        println!("{}", serde_json::to_string_pretty(&r)?);
        return Ok(());
    }
    let n = crate::art::thousands;
    println!("Phonton record");
    println!("  verified runs   {} of {}", n(r.verified_runs), n(r.runs));
    println!("  streak          {} (best {})", r.streak, r.best_streak);
    println!("  tokens local    {}", n(r.local_tokens));
    println!("  tokens cloud    {}", n(r.cloud_tokens));
    println!("  tokens unknown  {}", n(r.unknown_tokens));
    println!("Only finished runs count. Unverified or failed runs end the streak.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_finished_runs_do_not_lose_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    add_run_at(&path, Outcome::Verified, 10, TokenOrigin::ManagedLocal).unwrap()
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let record: Record = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(
            (record.runs, record.streak, record.local_tokens),
            (8, 8, 80)
        );
    }

    #[test]
    fn external_and_old_receipts_never_claim_local_inference() {
        use phonton_types::local_run::RuntimeOrigin;
        assert_eq!(
            TokenOrigin::from(RuntimeOrigin::ManagedVerified),
            TokenOrigin::ManagedLocal
        );
        for origin in [RuntimeOrigin::ExternalUnverified, RuntimeOrigin::Unknown] {
            let mut r = Record::default();
            r.add(Outcome::Verified, 99, origin.into());
            assert_eq!(
                (r.local_tokens, r.cloud_tokens, r.unknown_tokens),
                (0, 0, 99)
            );
        }
        let old: Record = serde_json::from_str(r#"{"local_tokens":12}"#).unwrap();
        assert_eq!((old.local_tokens, old.unknown_tokens), (12, 0));
    }

    #[test]
    fn only_verified_runs_extend_the_streak() {
        let mut r = Record::default();
        r.add(Outcome::Verified, 300, TokenOrigin::ManagedLocal);
        r.add(Outcome::Verified, 200, TokenOrigin::ManagedLocal);
        r.add(Outcome::Unverified, 50, TokenOrigin::Hosted);
        r.add(Outcome::Verified, 10, TokenOrigin::ManagedLocal);
        assert_eq!((r.runs, r.verified_runs), (4, 3));
        assert_eq!((r.streak, r.best_streak), (1, 2));
        assert_eq!((r.local_tokens, r.cloud_tokens), (510, 50));
        r.add(Outcome::Failed, 0, TokenOrigin::ManagedLocal);
        assert_eq!(r.streak, 0);
    }
}
