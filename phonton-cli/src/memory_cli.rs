use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use phonton_memory::MemoryStore;
use phonton_store::Store;
use phonton_types::MemoryRecord;
use serde::Serialize;

#[derive(Serialize)]
struct MemoryEntryJson {
    id: i64,
    kind: String,
    pinned: bool,
    topic: String,
    created_at: u64,
    record: MemoryRecord,
}

pub async fn run(args: &[String]) -> Result<i32> {
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print_help();
        return Ok(0);
    }

    let store = Arc::new(Mutex::new(open_persistent_store()?));
    let memory = MemoryStore::new(store).await;

    match args[0].as_str() {
        "list" => list(&memory, &args[1..]).await,
        "edit" => edit(&memory, &args[1..]).await,
        "delete" | "rm" => delete(&memory, &args[1..]).await,
        "pin" => set_pinned(&memory, &args[1..], true).await,
        "unpin" => set_pinned(&memory, &args[1..], false).await,
        other => {
            eprintln!("unknown memory command: {other}");
            print_help();
            Ok(2)
        }
    }
}

async fn list(memory: &MemoryStore, args: &[String]) -> Result<i32> {
    let mut json = false;
    let mut kind: Option<String> = None;
    let mut topic: Option<String> = None;
    let mut limit = 50usize;
    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--kind" => {
                i += 1;
                kind = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow!("--kind needs a value"))?
                        .clone(),
                );
            }
            "--topic" => {
                i += 1;
                topic = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow!("--topic needs a value"))?
                        .clone(),
                );
            }
            "--limit" => {
                i += 1;
                limit = args
                    .get(i)
                    .ok_or_else(|| anyhow!("--limit needs a value"))?
                    .parse()?;
            }
            other => return Err(anyhow!("unexpected argument for memory list: {other}")),
        }
        i += 1;
    }

    let entries = memory.list(kind, topic, limit).await?;
    if json {
        let out: Vec<MemoryEntryJson> = entries
            .into_iter()
            .map(|e| MemoryEntryJson {
                id: e.id,
                kind: e.kind,
                pinned: e.pinned,
                topic: e.topic,
                created_at: e.created_at,
                record: e.record,
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }

    if entries.is_empty() {
        println!("No memory records.");
        return Ok(0);
    }
    for entry in entries {
        let pin = if entry.pinned { " pinned" } else { "" };
        println!(
            "#{} {}{} - {}",
            entry.id,
            entry.kind,
            pin,
            summarize(&entry.record)
        );
    }
    Ok(0)
}

async fn edit(memory: &MemoryStore, args: &[String]) -> Result<i32> {
    if args.len() < 2 {
        return Err(anyhow!("usage: phonton memory edit <id> <text>"));
    }
    let id: i64 = args[0].parse()?;
    let text = args[1..].join(" ");
    let Some(entry) = memory.get(id).await? else {
        eprintln!("memory record #{id} not found");
        return Ok(1);
    };
    let updated = replace_primary_text(entry.record, text);
    if memory.update(id, updated).await? {
        println!("Updated memory record #{id}.");
        Ok(0)
    } else {
        eprintln!("memory record #{id} not found");
        Ok(1)
    }
}

async fn delete(memory: &MemoryStore, args: &[String]) -> Result<i32> {
    let id = parse_one_id(args, "phonton memory delete <id>")?;
    if memory.delete(id).await? {
        println!("Deleted memory record #{id}.");
        Ok(0)
    } else {
        eprintln!("memory record #{id} not found");
        Ok(1)
    }
}

async fn set_pinned(memory: &MemoryStore, args: &[String], pinned: bool) -> Result<i32> {
    let usage = if pinned {
        "phonton memory pin <id>"
    } else {
        "phonton memory unpin <id>"
    };
    let id = parse_one_id(args, usage)?;
    if memory.set_pinned(id, pinned).await? {
        println!(
            "{} memory record #{id}.",
            if pinned { "Pinned" } else { "Unpinned" }
        );
        Ok(0)
    } else {
        eprintln!("memory record #{id} not found");
        Ok(1)
    }
}

fn parse_one_id(args: &[String], usage: &str) -> Result<i64> {
    if args.len() != 1 {
        return Err(anyhow!("usage: {usage}"));
    }
    Ok(args[0].parse()?)
}

fn replace_primary_text(record: MemoryRecord, text: String) -> MemoryRecord {
    match record {
        MemoryRecord::Decision { title, task_id, .. } => MemoryRecord::Decision {
            title,
            body: text,
            task_id,
        },
        MemoryRecord::Constraint { statement, .. } => MemoryRecord::Constraint {
            statement,
            rationale: text,
        },
        MemoryRecord::RejectedApproach { summary, .. } => MemoryRecord::RejectedApproach {
            summary,
            reason: text,
        },
        MemoryRecord::Convention { scope, .. } => MemoryRecord::Convention { rule: text, scope },
    }
}

fn summarize(record: &MemoryRecord) -> String {
    match record {
        MemoryRecord::Decision { title, body, .. } => format!("{title}: {body}"),
        MemoryRecord::Constraint {
            statement,
            rationale,
        } => format!("{statement}: {rationale}"),
        MemoryRecord::RejectedApproach { summary, reason } => format!("{summary}: {reason}"),
        MemoryRecord::Convention { rule, scope } => match scope {
            Some(scope) => format!("{rule} ({scope})"),
            None => rule.clone(),
        },
    }
}

fn print_help() {
    println!(
        "USAGE:\n  \
         phonton memory list [--json] [--kind <kind>] [--topic <text>] [--limit <n>]\n  \
         phonton memory edit <id> <text>\n  \
         phonton memory delete <id>\n  \
         phonton memory pin <id>\n  \
         phonton memory unpin <id>\n\n\
         KINDS:\n  \
         Decision, Constraint, RejectedApproach, Convention"
    );
}

fn default_store_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".phonton").join("store.sqlite3"))
}

fn open_persistent_store() -> Result<Store> {
    let path =
        default_store_path().ok_or_else(|| anyhow!("could not determine ~/.phonton path"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Store::open(path)
}
