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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Verified,
    Unverified,
    Failed,
}

impl Record {
    pub fn add(&mut self, outcome: Outcome, tokens: u64, local: bool) {
        self.runs += 1;
        if local {
            self.local_tokens += tokens;
        } else {
            self.cloud_tokens += tokens;
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

/// Add one finished run and save. Returns the updated record.
// ponytail: last-writer-wins on concurrent runs; add a lock if two engines
// finishing in the same millisecond ever matters.
pub fn add_run(outcome: Outcome, tokens: u64, local: bool) -> Record {
    let mut record = load();
    record.add(outcome, tokens, local);
    if let Some(path) = path() {
        let tmp = path.with_extension("json.tmp");
        if let Ok(bytes) = serde_json::to_vec_pretty(&record) {
            let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path))
                .and_then(|_| std::fs::write(&tmp, bytes))
                .and_then(|_| std::fs::rename(&tmp, &path));
        }
    }
    record
}

/// Count a finished local-harness run (desktop or `phonton goal --local`).
pub fn add_local_receipt(receipt: &phonton_types::local_run::LocalRunReceipt) -> Record {
    if receipt.candidates.is_empty() {
        // Stopped before any candidate (admission, baseline): nothing ran.
        return load();
    }
    let tokens = receipt
        .candidates
        .iter()
        .map(|c| c.input_tokens.unwrap_or(0) + c.output_tokens.unwrap_or(0))
        .sum();
    let outcome = match receipt.state.as_str() {
        "review_ready" => Outcome::Verified,
        "review_unverified" => Outcome::Unverified,
        _ => Outcome::Failed,
    };
    add_run(outcome, tokens, true)
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
    println!("Only finished runs count. Unverified or failed runs end the streak.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_verified_runs_extend_the_streak() {
        let mut r = Record::default();
        r.add(Outcome::Verified, 300, true);
        r.add(Outcome::Verified, 200, true);
        r.add(Outcome::Unverified, 50, false);
        r.add(Outcome::Verified, 10, true);
        assert_eq!((r.runs, r.verified_runs), (4, 3));
        assert_eq!((r.streak, r.best_streak), (1, 2));
        assert_eq!((r.local_tokens, r.cloud_tokens), (510, 50));
        r.add(Outcome::Failed, 0, true);
        assert_eq!(r.streak, 0);
    }
}
